use config;

use infer_schema_internals::*;
use std::error::Error;
use std::fmt::{self, Display, Formatter, Write};
use std::io::{self, stdout};
use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};

pub enum Filtering {
    Whitelist(Vec<TableName>),
    Blacklist(Vec<TableName>),
    None,
}

impl Default for Filtering {
    fn default() -> Self {
        Filtering::None
    }
}

impl Filtering {
    pub fn should_ignore_table(&self, name: &TableName) -> bool {
        use self::Filtering::*;

        match *self {
            Whitelist(ref names) => !names.contains(name),
            Blacklist(ref names) => names.contains(name),
            None => false,
        }
    }
}

pub fn run_print_schema(
    database_url: &str,
    config: &config::PrintSchema,
) -> Result<(), Box<Error>> {
    output_schema(database_url, config, &mut stdout())
}

pub fn output_schema<W: io::Write>(
    database_url: &str,
    config: &config::PrintSchema,
    out: &mut W,
) -> Result<(), Box<Error>> {
    let table_names = load_table_names(database_url, config.schema_name())?
        .into_iter()
        .filter(|t| !config.filter.should_ignore_table(t))
        .collect::<Vec<_>>();
    let foreign_keys = load_foreign_key_constraints(database_url, config.schema_name())?;
    let foreign_keys =
        remove_unsafe_foreign_keys_for_codegen(database_url, &foreign_keys, &table_names);
    let table_data = table_names
        .into_iter()
        .map(|t| load_table_data(database_url, t))
        .collect::<Result<_, Box<Error>>>()?;
    let definitions = TableDefinitions {
        tables: table_data,
        fk_constraints: foreign_keys,
        include_docs: config.with_docs,
        import_types: config.import_types(),
    };

    if let Some(schema_name) = config.schema_name() {
        write!(out, "{}", ModuleDefinition(schema_name, definitions))?;
    } else {
        write!(out, "{}", definitions)?;
    }
    Ok(())
}

struct ModuleDefinition<'a>(&'a str, TableDefinitions<'a>);

impl<'a> Display for ModuleDefinition<'a> {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        {
            let mut out = PadAdapter::new(f);
            writeln!(out, "pub mod {} {{", self.0)?;
            write!(out, "{}", self.1)?;
        }
        writeln!(f, "}}")?;
        Ok(())
    }
}

struct TableDefinitions<'a> {
    tables: Vec<TableData>,
    fk_constraints: Vec<ForeignKeyConstraint>,
    include_docs: bool,
    import_types: Option<&'a [String]>,
}

impl<'a> Display for TableDefinitions<'a> {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        let mut is_first = true;
        for table in &self.tables {
            if is_first {
                is_first = false;
            } else {
                write!(f, "\n")?;
            }
            writeln!(
                f,
                "{}",
                TableDefinition {
                    table,
                    include_docs: self.include_docs,
                    import_types: self.import_types,
                }
            )?;
        }

        if !self.fk_constraints.is_empty() {
            write!(f, "\n")?;
        }

        for foreign_key in &self.fk_constraints {
            writeln!(f, "{}", Joinable(foreign_key))?;
        }

        if self.tables.len() > 1 {
            write!(f, "\nallow_tables_to_appear_in_same_query!(")?;
            {
                let mut out = PadAdapter::new(f);
                write!(out, "\n")?;
                for table in &self.tables {
                    writeln!(out, "{},", table.name.name)?;
                }
            }
            writeln!(f, ");")?;
        }

        Ok(())
    }
}

struct TableDefinition<'a> {
    table: &'a TableData,
    import_types: Option<&'a [String]>,
    include_docs: bool,
}

impl<'a> Display for TableDefinition<'a> {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        write!(f, "table! {{")?;
        {
            let mut out = PadAdapter::new(f);
            write!(out, "\n")?;

            if let Some(types) = self.import_types {
                for import in types {
                    writeln!(out, "use {};", import)?;
                }
                write!(out, "\n")?;
            }

            if self.include_docs {
                for d in self.table.docs.lines() {
                    writeln!(out, "///{}{}", if d.is_empty() { "" } else { " " }, d)?;
                }
            }

            write!(out, "{} (", self.table.name)?;
            for (i, pk) in self.table.primary_key.iter().enumerate() {
                if i != 0 {
                    write!(out, ", ")?;
                }
                write!(out, "{}", pk)?;
            }

            write!(
                out,
                ") {}",
                ColumnDefinitions {
                    columns: &self.table.column_data,
                    include_docs: self.include_docs,
                }
            )?;
        }
        write!(f, "}}")?;
        Ok(())
    }
}

struct ColumnDefinitions<'a> {
    columns: &'a [ColumnDefinition],
    include_docs: bool,
}

impl<'a> Display for ColumnDefinitions<'a> {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        {
            let mut out = PadAdapter::new(f);
            writeln!(out, "{{")?;
            for column in self.columns {
                if self.include_docs {
                    for d in column.docs.lines() {
                        writeln!(out, "///{}{}", if d.is_empty() { "" } else { " " }, d)?;
                    }
                }
                if let Some(ref rust_name) = column.rust_name {
                    writeln!(out, r#"#[sql_name = "{}"]"#, column.sql_name)?;
                    writeln!(out, "{} -> {},", rust_name, column.ty)?;
                } else {
                    writeln!(out, "{} -> {},", column.sql_name, column.ty)?;
                }
            }
        }
        writeln!(f, "}}")?;
        Ok(())
    }
}

struct Joinable<'a>(&'a ForeignKeyConstraint);

impl<'a> Display for Joinable<'a> {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        write!(
            f,
            "joinable!({} -> {} ({}));",
            self.0.child_table.name, self.0.parent_table.name, self.0.foreign_key,
        )
    }
}

/// Lifted directly from libcore/fmt/builders.rs
struct PadAdapter<'a, 'b: 'a> {
    fmt: &'a mut Formatter<'b>,
    on_newline: bool,
}

impl<'a, 'b: 'a> PadAdapter<'a, 'b> {
    fn new(fmt: &'a mut Formatter<'b>) -> PadAdapter<'a, 'b> {
        PadAdapter {
            fmt: fmt,
            on_newline: false,
        }
    }
}

impl<'a, 'b: 'a> Write for PadAdapter<'a, 'b> {
    fn write_str(&mut self, mut s: &str) -> fmt::Result {
        while !s.is_empty() {
            let on_newline = self.on_newline;

            let split = match s.find('\n') {
                Some(pos) => {
                    self.on_newline = true;
                    pos + 1
                }
                None => {
                    self.on_newline = false;
                    s.len()
                }
            };

            let to_write = &s[..split];
            if on_newline && to_write != "\n" {
                self.fmt.write_str("    ")?;
            }
            self.fmt.write_str(to_write)?;

            s = &s[split..];
        }

        Ok(())
    }
}

impl<'de> Deserialize<'de> for Filtering {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct FilteringVisitor;

        impl<'de> Visitor<'de> for FilteringVisitor {
            type Value = Filtering;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("either a whitelist or a blacklist")
            }

            fn visit_map<V>(self, mut map: V) -> Result<Self::Value, V::Error>
            where
                V: MapAccess<'de>,
            {
                let mut whitelist = None;
                let mut blacklist = None;
                while let Some((key, value)) = map.next_entry()? {
                    match key {
                        "whitelist" => {
                            if whitelist.is_some() {
                                return Err(de::Error::duplicate_field("whitelist"));
                            }
                            whitelist = Some(value);
                        }
                        "blacklist" => {
                            if blacklist.is_some() {
                                return Err(de::Error::duplicate_field("blacklist"));
                            }
                            blacklist = Some(value);
                        }
                        _ => return Err(de::Error::unknown_field(key, &["whitelist", "blacklist"])),
                    }
                }
                match (whitelist, blacklist) {
                    (Some(_), Some(_)) => Err(de::Error::duplicate_field("blacklist")),
                    (Some(w), None) => Ok(Filtering::Whitelist(w)),
                    (None, Some(b)) => Ok(Filtering::Blacklist(b)),
                    (None, None) => Ok(Filtering::None),
                }
            }
        }

        deserializer.deserialize_map(FilteringVisitor)
    }
}
