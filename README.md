# pgcopy

`pgcopy` экспортирует выбранные таблицы, вьюшки и материальные вьюшки из БД PostgreSQL в один файл, с последующей возможностью импортировать эти обьекты из файла в другую БД в качестве таблиц.

Экспорт:
![Alt text](images/export.gif "import")

Импорт:
![Alt text](images/import.gif "import")

Основной сценарий:

- `export`: снимает DDL + данные в bundle-файл.
- `import`: восстанавливает bundle в целевую БД в виде обычных таблиц.
- `info`: показывает метаинформацию bundle без импорта в БД.

## Быстрый старт

1. Создайте `config.toml`.
2. Передайте параметры подключения к PostgreSQL через CLI или env.
3. Выполните `export`.
4. На целевой стороне выполните `import` полученного на стадии 3 файла.

## Команды

```bash
pgcopy export --config <path/to/config.toml> --out <path/to/bundle> [--concurrency N] [--quiet] [--no-progress]
pgcopy import --in <path/to/bundle> [--mode replace|append] [--concurrency N] [--quiet] [--no-progress]
pgcopy info --in <path/to/bundle> [--format text|json] [--objects] [--quiet]
```

`--mode` для импорта:

- `replace` (по умолчанию): дропает целевую таблицу, затем создает заново и загружает данные.
  Операция выполняется атомарно на уровне объекта (`BEGIN/COMMIT`): при ошибке во время `replace`
  изменения по этому объекту откатываются (`ROLLBACK`).
- `append`: если таблица есть, проверяет совместимость и дозаписывает данные.

Поддерживается также алиас `--concurency` (без второй `r`) для `--concurrency`.

`info`:

- `--format text` (по умолчанию): человекочитаемый вывод.
- `--format json`: машинночитаемый вывод.
- `--objects`: печатает метаданные по каждому объекту из manifest.

Служебные флаги:

- `--quiet`: отключает служебный вывод (баннер запуска и progress bars).
- `--no-progress`: отключает только progress bars (удобно для CI-логов).

## Приоритет источников параметров

Приоритет значений: `CLI > TOML-конфиг > переменные окружения`.

## Подключение к PostgreSQL

Можно задавать через CLI:

- `--host`
- `--port`
- `--dbname`
- `--username` (alias: `--user`)
- `--pgpassword`

Или через стандартные переменные окружения PostgreSQL:

- `PGHOST`
- `PGPORT` (по умолчанию `5432`, если не задана)
- `PGDATABASE`
- `PGUSER`
- `PGPASSWORD`

Для параметров подключения PostgreSQL используется приоритет: CLI-параметры выше env.

Пример через env:

```bash
export PGHOST=127.0.0.1
export PGPORT=5432
export PGDATABASE=app_db
export PGUSER=app_user
export PGPASSWORD=secret
```

Пример через CLI:

```bash
pgcopy export \
  --config ./config.toml \
  --out ./bundle.tar.zst \
  --host 127.0.0.1 \
  --port 5432 \
  --dbname app_db \
  --username app_user \
  --pgpassword secret
```

## Защита bundle паролем

Bundle можно шифровать/расшифровывать паролем:

- через `--password`
- или через env `PASSWORD` (если `--password` не указан)

Если пароль не задан ни там, ни там, bundle создается/читается без шифрования.

Пример экспорта с паролем:

```bash
pgcopy export --config ./config.toml --out ./bundle.enc --password "strong-passphrase"
```

Пример импорта с паролем из env:

```bash
export PASSWORD="strong-passphrase"
pgcopy import --in ./bundle.enc --mode replace
```

Если bundle зашифрован, а пароль не передан, импорт завершится ошибкой.
Для `info` это правило такое же: для зашифрованного bundle нужно передать `--password` или `PASSWORD`.

Примеры `info`:

```bash
pgcopy info --in ./bundle.tar.zst
pgcopy info --in ./bundle.tar.zst --objects
pgcopy info --in ./bundle.enc --password "strong-passphrase" --format json
```

## Формат `config.toml`

Минимальный пример:

```toml
[general]
data_format = "binary"
compression = "zstd"
consistent_snapshot = true
concurrency = 4

[[objects]]
select = "select * from public.orders"

[[objects]]
select = "select id, total_amount, created_at from public.orders"
target_schema = "archive"
target_name = "orders_snapshot"

[[objects]]
select = "select day, region, revenue from reporting.v_sales_daily"
target_schema = "snapshots"
target_name = "sales_daily"

[[objects]]
select = "select * from public.orders where created_at >= date '2026-01-01'"
```

Поддерживаемые формы `select`:

- `select * from schema.object`
- `select col1, col2 from schema.object`
- `select * except (col1, col2) from schema.object`
- `select ... from schema.object where <predicate>`
- `select ... from schema.object order by <expr>`
- `select ... from schema.object limit <N>`
- комбинация в порядке: `where ... order by ... limit ...`

Ограничения DSL:

- только один источник `schema.object`
- без `JOIN/GROUP BY`
- идентификаторы в `FROM/SELECT/EXCEPT` можно указывать:
  - unquoted (`schema.object`, `col_name`) — автоматически нормализуются в lower-case
  - quoted (`"123schema"."Orders"`, `"Col Name"`) — сохраняются как есть
- `WHERE/ORDER BY` вставляются в нормализованный SQL как есть
- `LIMIT` поддерживается как неотрицательное целое число
- клаузa должна идти в порядке `WHERE -> ORDER BY -> LIMIT`
- `;` в DSL запрещен

Важно:

- для `table`, `view` и `materialized view` `target_schema/target_name` необязательны; по умолчанию берутся source schema/name
- `data_format` поддерживает `binary` и `csv`
- для `binary` проверка совместимости COPY требует одинаковую мажорную версию PostgreSQL между source и target
- для `csv` проверка по major-версии PostgreSQL не применяется
- для колонок с `DEFAULT nextval(...::regclass)` создается target-local sequence, и после импорта
  sequence синхронизируется с `MAX(column)` загруженных данных
- `general.concurrency` задает число объектов, обрабатываемых параллельно (по умолчанию `1`)
- `export` принимает `--concurrency` (алиас `--concurency`) и разрешает значение так:
  `CLI > general.concurrency из TOML > PGCOPY_CONCURRENCY > 1`
- для `import` параллелизм задается CLI-флагом `--concurrency` (по умолчанию `1`)

Пример с CSV:

```toml
[general]
data_format = "csv"
compression = "zstd"
consistent_snapshot = true
concurrency = 2
```

## Типовой workflow

Экспорт:

```bash
pgcopy export --config ./config.toml --out ./bundle.tar.zst
```

Импорт:

```bash
pgcopy import --in ./bundle.tar.zst --mode replace
```

Параллельный режим:

```toml
[general]
concurrency = 4
```

```bash
pgcopy export --config ./config.toml --out ./bundle.tar.zst --concurrency 4
pgcopy import --in ./bundle.tar.zst --mode replace --concurrency 4
```

## Сборка проекта

```bash
cargo build --release
```

Бинарник:

```bash
./target/release/pgcopy --help
```
