use anyhow::{ensure, Result};
use rusqlite::Connection;

pub const SCHEMA: &str = include_str!("schema.sql");

pub struct Gap {
    table: String,
    absent: bool,
    columns: Vec<(String, String, bool)>,
}

pub fn inspect(db: &Connection) -> Result<(Vec<Gap>, bool)> {
    // Use SQLite's own parser for column names, types and primary keys.
    let template = Connection::open_in_memory()?;
    template.execute_batch(SCHEMA)?;
    let mut gaps = Vec::new();
    let mut recognized = false;
    let mut tables = template.prepare(
        "SELECT name FROM sqlite_schema WHERE type='table' AND name <> 'sqlite_sequence'",
    )?;
    for table in tables.query_map([], |r| r.get::<_, String>(0))? {
        let table = table?;
        let mut actual = db.prepare("SELECT name FROM pragma_table_info(?1, 'main')")?;
        let actual = actual
            .query_map([&table], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        recognized |= !actual.is_empty();
        let mut columns =
            template.prepare("SELECT name, type, pk OR \"notnull\" FROM pragma_table_info(?1)")?;
        let mut missing = Vec::new();
        for col in columns.query_map([&table], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))? {
            let (name, kind, required) = col?;
            if !actual.contains(&name) {
                missing.push((name, kind, required));
            }
        }
        if !missing.is_empty() {
            gaps.push(Gap {
                table,
                absent: actual.is_empty(),
                columns: missing,
            });
        }
    }
    if !recognized {
        let has_tables: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM main.sqlite_schema WHERE name NOT GLOB 'sqlite_*')",
            [],
            |r| r.get(0),
        )?;
        ensure!(!has_tables, "unrecognized router.db schema");
    }
    Ok((gaps, recognized))
}

impl Gap {
    pub fn project(&self, db: &Connection) -> Result<()> {
        let columns = self
            .columns
            .iter()
            .map(|(name, _, _)| format!("NULL AS {name}"))
            .collect::<Vec<_>>()
            .join(", ");
        let table = &self.table;
        let select = if self.absent {
            format!("SELECT {columns} WHERE 0")
        } else {
            format!("SELECT *, {columns} FROM main.{table}")
        };
        db.execute_batch(&format!("CREATE TEMP VIEW {table} AS {select}"))?;
        Ok(())
    }

    pub fn upgrade(&self, db: &Connection) -> Result<()> {
        if !self.absent {
            for (name, kind, required) in &self.columns {
                ensure!(
                    !required,
                    "cannot reconstruct required column {}.{name}",
                    self.table
                );
                // Python ALTER adds bare nullable columns; backfill assigns the durable values.
                db.execute_batch(&format!(
                    "ALTER TABLE {} ADD COLUMN {name} {kind}",
                    self.table
                ))?;
            }
        }
        Ok(())
    }
}
