#!/usr/bin/env bash

# Скрипт собирает два вида данных:
#   1. perf record — сэмплы стека вызовов для поиска горячих функций;
#   2. perf stat   — текстовый отчёт со сводными аппаратными счётчиками
#      и временем выполнения.
#
# По умолчанию профилируется типичный последовательный импорт:
#   pgcopy import --in bundle.tar.zst --host localhost --dbname gas_dwh \
#     --username pguser
#
# Пароль берётся из PGCOPY_PROFILE_PGPASSWORD или PGPASSWORD и передаётся
# дочернему процессу только через окружение, поэтому не попадает в argv/perf.data.
#
# Настройки можно переопределить переменными окружения:
#   PGCOPY_PROFILE_BUNDLE, PGCOPY_PROFILE_HOST, PGCOPY_PROFILE_PORT,
#   PGCOPY_PROFILE_DBNAME, PGCOPY_PROFILE_USERNAME,
#   PGCOPY_PROFILE_PGPASSWORD, PGCOPY_PROFILE_CONCURRENCY,
#   PGCOPY_PROFILE_REPEATS, PGCOPY_PROFILE_DIR и PGCOPY_PROFILE_NAME.
#
# Произвольную команду pgcopy можно передать после `--`, например:
#   ./profiling.sh -- info --in vv.tar.zst
#
# Пример профилирования export из run_export.sh:
#   PGHOST=localhost PGPORT=5432 PGUSER=pguser PGPASSWORD=pguser \
#     PGDATABASE=gas_dwh PGCOPY_PROFILE_NAME=pgcopy-export-c1 \
#     ./profiling.sh -- --quiet export --config export/config.toml \
#       --out out/profiling-export.tar.zst --concurrency 1
#
# Полный запуск по умолчанию выполняет четыре импорта: один для perf record
# и три повтора perf stat. Режим import=replace делает повторы независимыми
# от наличия целевых таблиц.

# Остановить сценарий при первой ошибке, обращении к неопределённой переменной
# или ошибке внутри конвейера команд.
set -Eeuo pipefail

# Все пути вычисляются относительно расположения скрипта, поэтому его можно
# запускать не только из корня проекта.
readonly PROJECT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
readonly PROFILE_DIR="${PGCOPY_PROFILE_DIR:-$PROJECT_DIR/out/profiles}"
readonly BINARY="$PROJECT_DIR/target/profiling/pgcopy"
readonly BUNDLE="${PGCOPY_PROFILE_BUNDLE:-$PROJECT_DIR/bundle.tar.zst}"
readonly DB_HOST="${PGCOPY_PROFILE_HOST:-localhost}"
readonly DB_PORT="${PGCOPY_PROFILE_PORT:-5432}"
readonly DB_NAME="${PGCOPY_PROFILE_DBNAME:-gas_dwh}"
readonly DB_USER="${PGCOPY_PROFILE_USERNAME:-pguser}"
readonly DB_PASSWORD="${PGCOPY_PROFILE_PGPASSWORD:-${PGPASSWORD:-pguser}}"
readonly IMPORT_CONCURRENCY="${PGCOPY_PROFILE_CONCURRENCY:-1}"
readonly PERF_STAT_REPEATS="${PGCOPY_PROFILE_REPEATS:-3}"

# task-clock показывает суммарное процессорное время. Программные счётчики
# context-switches, cpu-migrations и page-faults помогают обнаружить накладные
# расходы планировщика и памяти. Остальные события характеризуют эффективность
# выполнения инструкций, ветвлений и работы с кэшем процессора.
readonly PERF_EVENTS="task-clock,context-switches,cpu-migrations,page-faults,cycles,instructions,branches,branch-misses,cache-references,cache-misses"

if (($# > 0)) && [[ "$1" == "--" ]]; then
    shift
fi

if (($# > 0)); then
    readonly DEFAULT_PROFILE_NAME="pgcopy-custom"
    readonly -a PROFILE_COMMAND=("$BINARY" "$@")
else
    if [[ ! "$IMPORT_CONCURRENCY" =~ ^[1-9][0-9]*$ ]]; then
        printf 'error: PGCOPY_PROFILE_CONCURRENCY must be a positive integer\n' >&2
        exit 2
    fi
    if [[ ! -r "$BUNDLE" ]]; then
        printf 'error: profiling bundle is not readable: %s\n' "$BUNDLE" >&2
        exit 2
    fi

    readonly DEFAULT_PROFILE_NAME="pgcopy-import-c$IMPORT_CONCURRENCY"
    readonly -a PROFILE_COMMAND=(
        "$BINARY"
        --quiet
        import
        --in "$BUNDLE"
        --mode replace
        --concurrency "$IMPORT_CONCURRENCY"
        --host "$DB_HOST"
        --port "$DB_PORT"
        --dbname "$DB_NAME"
        --username "$DB_USER"
    )
fi

readonly PROFILE_NAME="${PGCOPY_PROFILE_NAME:-$DEFAULT_PROFILE_NAME}"

if [[ ! "$PERF_STAT_REPEATS" =~ ^[1-9][0-9]*$ ]]; then
    printf 'error: PGCOPY_PROFILE_REPEATS must be a positive integer\n' >&2
    exit 2
fi

if [[ ! "$PROFILE_NAME" =~ ^[a-zA-Z0-9][a-zA-Z0-9._-]*$ ]]; then
    printf 'error: PGCOPY_PROFILE_NAME contains unsupported characters\n' >&2
    exit 2
fi

if ! command -v perf >/dev/null 2>&1; then
    printf 'error: perf is not installed or not available in PATH\n' >&2
    exit 127
fi

# Записать профиль вызовов. Событие cycles:u учитывает процессорные циклы только
# в пользовательском коде, а frame pointers позволяют восстановить стек вызовов.
record_profile() {
    local output="$1"

    perf record \
        --freq 499 \
        --event cycles:u \
        --call-graph fp \
        --output "$output" \
        -- "${PROFILE_COMMAND[@]}"
}

# Собрать сводную статистику несколько раз для более устойчивого результата.
# perf stat записывает среднее значение и относительный разброс в указанный файл.
collect_stats() {
    local output="$1"

    # В отличие от perf record, perf stat не сохраняет предыдущий файл сам.
    # Оставляем одно предыдущее измерение рядом с текущим отчётом.
    if [[ -e "$output" ]]; then
        mv -f -- "$output" "$output.old"
    fi

    perf stat \
        --repeat "$PERF_STAT_REPEATS" \
        --event "$PERF_EVENTS" \
        --output "$output" \
        -- "${PROFILE_COMMAND[@]}"
}

cd -- "$PROJECT_DIR"
mkdir -p -- "$PROFILE_DIR"

# Профиль profiling наследует release-оптимизации и сохраняет отладочные
# символы. RUSTFLAGS применяется ко всему dependency graph, чтобы frame
# pointers были доступны не только в коде pgcopy. Эта команда выполняется
# до perf и не входит ни в одно измерение.
readonly PROFILING_RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }-C force-frame-pointers=yes"
RUSTFLAGS="$PROFILING_RUSTFLAGS" cargo build \
    --quiet \
    --locked \
    --profile profiling \
    --package pgcopy

# Экспортируем пароль после сборки: он наследуется pgcopy, но не становится
# аргументом командной строки cargo или perf.
export PGPASSWORD="$DB_PASSWORD"

readonly PERF_DATA="$PROFILE_DIR/$PROFILE_NAME.perf.data"
readonly PERF_REPORT="$PROFILE_DIR/$PROFILE_NAME.perf-report.txt"
readonly PERF_STAT="$PROFILE_DIR/$PROFILE_NAME.perf-stat.txt"

# Если perf.data уже существует, perf переносит предыдущую версию в соседний
# файл с суффиксом .old.
record_profile "$PERF_DATA"
perf report \
    --stdio \
    --no-children \
    --call-graph none \
    --input "$PERF_DATA" \
    --sort dso,symbol \
    >"$PERF_REPORT"
collect_stats "$PERF_STAT"

printf 'perf data:   %s\n' "$PERF_DATA"
printf 'perf report: %s\n' "$PERF_REPORT"
printf 'perf stat:   %s\n' "$PERF_STAT"
