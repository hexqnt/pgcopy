# pgcopy

`pgcopy` — CLI-утилита, которая экспортирует выбранные объекты PostgreSQL (таблицы, представления и материализованные
представления) в один `bundle`-файл и затем импортирует их в другую БД **в виде обычных таблиц**.

## Содержание

- [Возможности](#возможности)
- [Демо](#демо)
- [Быстрый старт](#быстрый-старт)
- [Команды и флаги](#команды-и-флаги)
- [Подключение к PostgreSQL](#подключение-к-postgresql)
- [Шифрование bundle](#шифрование-bundle)
- [`config.toml`](#configtoml)
- [Типовой workflow](#типовой-workflow)
- [Сборка проекта](#сборка-проекта)

## Возможности

- `export`: снимает `DDL` + данные выбранных объектов в один `bundle` (по умолчанию `tar.zst`), опционально шифрует
  паролем.
- `import`: разворачивает `bundle` в целевую БД как таблицы (стратегии `replace`/`append`, режим `--ddl-only`).
- `info`: показывает метаданные `bundle` без подключения к PostgreSQL.
- Параллельная обработка объектов: `--concurrency` (для `export` и `import`).

## Демо

Экспорт:

```sh
export PGHOST=company-host
export PGPORT=5432
export PGUSER=pguser
export PGPASSWORD=pgpassword
export PGDATABASE=company-dwh
pgcopy export --config config.toml --out bundle.tar.zst
```

![Экспорт](images/export.gif "export")

Импорт:

```sh
pgcopy import --in bundle.tar.zst  --host localhost --dbname gas_dwh --username pguser --pgpassword pguser
```

![Импорт](images/import.gif "import")

## Быстрый старт

1. Составьте `config.toml` со списком объектов.
2. Укажите параметры подключения к PostgreSQL через CLI или env (см. ниже).
3. На исходной БД выполните экспорт:

   ```bash
   pgcopy export --config ./config.toml --out ./bundle.tar.zst
   ```

4. На целевой стороне выполните импорт:

   ```bash
   pgcopy import --in ./bundle.tar.zst --mode replace
   ```

## Команды и флаги

Синопсис:

```bash
pgcopy export --config <path/to/config.toml> --out <path/to/bundle> [--concurrency N] [--password PASSWORD] [--quiet] [--no-progress]
pgcopy import --in <path/to/bundle> [--mode replace|append] [--concurrency N] [--ddl-only] [--password PASSWORD] [--quiet] [--no-progress]
pgcopy info --in <path/to/bundle> [--format text|json] [--objects] [--password PASSWORD] [--quiet]
```

Подсказка: `pgcopy --help`, `pgcopy export --help`, `pgcopy import --help`, `pgcopy info --help`.

Глобальные флаги:

- `--quiet`: отключает служебный вывод (баннер запуска и progress bars).
- `--no-progress`: отключает только progress bars (удобно для CI-логов).

### `export`

- `--config`: путь к `config.toml`.
- `--out`: путь к выходному `bundle` (например `./bundle.tar.zst`).
- `--concurrency`: параллелизм экспорта (поддерживается алиас `--concurency` без второй `r`).
  Приоритет разрешения значения: `CLI > general.concurrency из TOML > PGCOPY_CONCURRENCY > 1`.
- `--password`: пароль для шифрования `bundle` (fallback: env `PASSWORD`).
- Параметры подключения к source PostgreSQL: см. раздел ниже.

### `import`

- `--in`: путь к входному `bundle`, созданному командой `export`.
- `--mode`: стратегия импорта при наличии целевой таблицы:
  - `replace` (по умолчанию): дропает целевую таблицу, затем создает заново и загружает данные. Операция выполняется
    атомарно на уровне объекта (`BEGIN/COMMIT`): при ошибке во время `replace` изменения по этому объекту откатываются
    (`ROLLBACK`).
  - `append`: если таблица уже есть, проверяет совместимость и дозаписывает данные.
- `--ddl-only`: выполняет только DDL-часть импорта (создание/подготовка таблиц), без загрузки данных.
- `--concurrency`: параллелизм импорта (количество объектов, обрабатываемых параллельно; алиас `--concurency`).
- `--password`: пароль для расшифровки `bundle` (fallback: env `PASSWORD`).
- Параметры подключения к target PostgreSQL: см. раздел ниже.

### `info`

- `--format text` (по умолчанию): человекочитаемый вывод.
- `--format json`: машинночитаемый вывод.
- `--objects`: печатает метаданные по каждому объекту из manifest.
- `--password`: пароль для расшифровки `bundle` (fallback: env `PASSWORD`).

## Подключение к PostgreSQL

Параметры подключения можно задавать через CLI-флаги:

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

Приоритет значений для подключения: `CLI > env`.

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

## Шифрование bundle

`bundle` можно шифровать/расшифровывать паролем:

- через `--password`
- или через env `PASSWORD` (если `--password` не указан)

Если пароль не задан ни там, ни там, `bundle` создается/читается без шифрования.

Пример экспорта с паролем:

```bash
pgcopy export --config ./config.toml --out ./bundle.enc --password "strong-passphrase"
```

Пример импорта с паролем из env:

```bash
export PASSWORD="strong-passphrase"
pgcopy import --in ./bundle.enc --mode replace
```

Если `bundle` зашифрован, а пароль не передан, `import`/`info` завершатся ошибкой.

## `config.toml`

Конфиг используется командой `export` и описывает список объектов для выгрузки и общие параметры экспорта.

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

[[objects]]
select = "select * from reporting.v_sales_daily"
export_as = "view"
```

### Общие настройки (`[general]`)

- `data_format`: `binary` (по умолчанию) или `csv`.
- `compression`: сейчас поддерживается только `zstd`.
- `consistent_snapshot`: включает согласованное чтение в одном `REPEATABLE READ` snapshot (по умолчанию `true`).
- `concurrency`: число параллельных workers экспорта (`>= 1`, по умолчанию `1`).

### Объекты (`[[objects]]`)

- `select`: строка ограниченного `select` DSL (см. ниже).
- `target_schema` и `target_name`: опционально переопределяют имя целевого объекта.
  Если задаете — задавайте **оба** поля, иначе конфиг не пройдет валидацию.
  Если не заданы — используются `schema/name` источника.
- `export_as`: `table` (по умолчанию) или `view`.
  - `table`: текущий режим snapshot/materialize (`SELECT -> CREATE TABLE + COPY`).
  - `view`: в bundle сохраняется `CREATE VIEW`, а зависимости этой view автоматически добавляются в экспорт как таблицы.
    Для `export_as = "view"` разрешен только вид `select * from schema.object` (без `WHERE/ORDER BY/LIMIT`).

### `select` DSL

Поддерживаемые формы:

- `select * from schema.object`
- `select col1, col2 from schema.object`
- `select * except (col1, col2) from schema.object`
- `select ... from schema.object where <predicate>`
- `select ... from schema.object order by <expr>`
- `select ... from schema.object limit <N>`
- комбинация в порядке: `where ... order by ... limit ...`

Ограничения:

- только один источник `schema.object`;
- без `JOIN/GROUP BY`;
- идентификаторы в `FROM/SELECT/EXCEPT` можно указывать:
  - unquoted (`schema.object`, `col_name`) — автоматически нормализуются в lower-case;
  - quoted (`"123schema"."Orders"`, `"Col Name"`) — сохраняются как есть;
- `WHERE/ORDER BY` вставляются в нормализованный SQL как есть;
- `LIMIT` поддерживается как неотрицательное целое число;
- клаузa должна идти в порядке `WHERE -> ORDER BY -> LIMIT`;
- `;` в DSL запрещен.

### Совместимость и нюансы

- Для `data_format = "binary"` проверка совместимости `COPY` требует одинаковую **мажорную** версию PostgreSQL между
  source и target.
- Для `data_format = "csv"` проверка по major-версии PostgreSQL не применяется.
- Для колонок с `DEFAULT nextval(...::regclass)` создается target-local sequence, и после импорта sequence
  синхронизируется с `MAX(column)` загруженных данных.
- Для bundle, содержащих объекты с `export_as = "view"`, импорт нужно запускать с `--concurrency 1`.
- `export` поддерживает параллелизм через `--concurrency` (алиас `--concurency`) и разрешает значение так:
  `CLI > general.concurrency из TOML > PGCOPY_CONCURRENCY > 1`.
- Для `import` параллелизм задается CLI-флагом `--concurrency` (по умолчанию `1`).

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
