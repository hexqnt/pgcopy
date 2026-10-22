macro_rules! selected_sources_sql {
    ($($query:literal),+ $(,)?) => {
        concat!(
            "WITH selected AS (\n\
                 SELECT schema_name, relation_name, source_index\n\
                 FROM ROWS FROM (\n\
                     pg_catalog.unnest($1::text[]),\n\
                     pg_catalog.unnest($2::text[])\n\
                 ) WITH ORDINALITY AS input(schema_name, relation_name, source_index)\n\
             )\n",
            $($query),+
        )
    };
}

pub(super) const RELATION_METADATA: &str = selected_sources_sql!(
    "SELECT
        c.relkind::text,
        CASE WHEN $3 THEN pg_catalog.obj_description(c.oid, 'pg_class') END,
        CASE WHEN $4 AND c.relkind <> 'v' AND c.reltuples >= 0
             THEN c.reltuples::bigint END,
        CASE WHEN c.relkind IN ('v', 'm')
             THEN pg_catalog.pg_get_viewdef(c.oid, true) END,
        selected.source_index
    FROM selected
    LEFT JOIN pg_catalog.pg_namespace n ON n.nspname = selected.schema_name
    LEFT JOIN pg_catalog.pg_class c
      ON c.relnamespace = n.oid AND c.relname = selected.relation_name
    ORDER BY selected.source_index
"
);

pub(super) const COLUMNS: &str = selected_sources_sql!(
    "SELECT
        a.attname,
        pg_catalog.format_type(a.atttypid, a.atttypmod),
        a.attnotnull,
        pg_catalog.pg_get_expr(ad.adbin, ad.adrelid),
        a.attidentity::text,
        a.attgenerated::text,
        CASE WHEN $3 THEN pg_catalog.col_description(c.oid, a.attnum) END,
        CASE WHEN t.typtype = 'd' THEN t.typbasetype ELSE t.oid END =
            'pg_catalog.bytea'::pg_catalog.regtype,
        COALESCE((
            SELECT pg_catalog.json_agg(e.enumlabel ORDER BY e.enumsortorder)
            FROM pg_catalog.pg_enum e
            WHERE e.enumtypid = CASE WHEN t.typtype = 'd'
                                     THEN t.typbasetype ELSE a.atttypid END
        ), '[]'::json)::text,
        selected.source_index
    FROM selected
    JOIN pg_catalog.pg_namespace n ON n.nspname = selected.schema_name
    JOIN pg_catalog.pg_class c
      ON c.relnamespace = n.oid AND c.relname = selected.relation_name
    JOIN pg_catalog.pg_attribute a ON a.attrelid = c.oid
    JOIN pg_catalog.pg_type t ON t.oid = a.atttypid
    LEFT JOIN pg_catalog.pg_attrdef ad
      ON ad.adrelid = a.attrelid AND ad.adnum = a.attnum
    WHERE a.attnum > 0
      AND NOT a.attisdropped
    ORDER BY selected.source_index, a.attnum
"
);

pub(super) const CONSTRAINTS: &str = selected_sources_sql!(
    "SELECT
        con.conname,
        con.contype::text,
        pg_catalog.pg_get_constraintdef(con.oid, true),
        foreign_ns.nspname,
        foreign_class.relname,
        selected.source_index
    FROM selected
    JOIN pg_catalog.pg_namespace n ON n.nspname = selected.schema_name
    JOIN pg_catalog.pg_class c
      ON c.relnamespace = n.oid AND c.relname = selected.relation_name
    JOIN pg_catalog.pg_constraint con ON con.conrelid = c.oid
    LEFT JOIN pg_catalog.pg_class foreign_class ON foreign_class.oid = con.confrelid
    LEFT JOIN pg_catalog.pg_namespace foreign_ns
      ON foreign_ns.oid = foreign_class.relnamespace
    WHERE con.contype IN ('p', 'u', 'f', 'c', 'x')
    ORDER BY selected.source_index,
             CASE con.contype
                 WHEN 'p' THEN 1 WHEN 'u' THEN 2 WHEN 'f' THEN 3
                 WHEN 'c' THEN 4 ELSE 5
             END,
             con.conname
"
);

macro_rules! statistics_sql {
    ($values:literal, $frequencies:literal) => {
        selected_sources_sql!(
            "SELECT
        stats.attname,
        stats.null_frac::float8,
        CASE WHEN stats.n_distinct >= 0 THEN stats.n_distinct::float8
             WHEN c.reltuples >= 0 THEN -stats.n_distinct::float8 * c.reltuples::float8 END,
        stats.avg_width,
        ",
            $values,
            ",
        ",
            $frequencies,
            ",
        selected.source_index
    FROM selected
    JOIN pg_catalog.pg_stats stats
      ON stats.schemaname = selected.schema_name AND stats.tablename = selected.relation_name
    JOIN pg_catalog.pg_namespace n ON n.nspname = selected.schema_name
    JOIN pg_catalog.pg_class c
      ON c.relnamespace = n.oid AND c.relname = selected.relation_name
    ORDER BY selected.source_index, stats.attname
",
        )
    };
}

pub(super) const STATISTICS: &str = statistics_sql!("NULL::text", "NULL::text");

pub(super) const STATISTICS_WITH_COMMON_VALUES: &str = statistics_sql!(
    "pg_catalog.to_json(stats.most_common_vals)::text",
    "pg_catalog.to_json(stats.most_common_freqs)::text"
);
