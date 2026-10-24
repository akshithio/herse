//! The first cell of a notebook about a data file: the code that loads it.

use crate::{Format, JsonShape, Sniffed};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Library {
    Pandas,
    Polars,
}

impl Library {
    pub fn label(self) -> &'static str {
        match self {
            Library::Pandas => "pandas",
            Library::Polars => "polars",
        }
    }
}

/// Python that loads `file` (a path as the notebook should write it,
/// usually the bare file name beside it) into `df` and shows it.
pub fn loading_code(file: &str, format: Format, sniffed: &Sniffed, library: Library) -> String {
    let path = py_str(file);
    // JSON that isn't a list of records: load it with `json` and build the
    // frame from the part Herse showed.
    if format == Format::Json
        && let Some(shape) = sniffed.json.as_ref().filter(|s| **s != JsonShape::Records)
    {
        let (import, build) = match (library, shape) {
            (Library::Pandas, JsonShape::Split) => ("import pandas as pd", "pd.DataFrame(data[\"data\"], columns=data[\"columns\"])".to_string()),
            (Library::Pandas, JsonShape::ColumnsByRow) => return format!("import pandas as pd\n\ndf = pd.read_json({path})\ndf"),
            (Library::Pandas, JsonShape::Wrapped(key)) => ("import pandas as pd", format!("pd.DataFrame(data[{}])", py_str(key))),
            (Library::Pandas, JsonShape::Other) => ("import pandas as pd", "pd.json_normalize(data)".to_string()),
            (Library::Pandas, _) => ("import pandas as pd", "pd.DataFrame(data)".to_string()),
            (Library::Polars, JsonShape::Split) => ("import polars as pl", "pl.DataFrame(data[\"data\"], schema=data[\"columns\"], orient=\"row\")".to_string()),
            (Library::Polars, JsonShape::ColumnsByRow) => (
                "import polars as pl",
                "pl.DataFrame({\"\": list(next(iter(data.values()), {})), **{c: list(v.values()) for c, v in data.items()}})".to_string(),
            ),
            (Library::Polars, JsonShape::Wrapped(key)) => ("import polars as pl", format!("pl.DataFrame(data[{}])", py_str(key))),
            (Library::Polars, JsonShape::Other) => ("import polars as pl", "pl.DataFrame([data])".to_string()),
            (Library::Polars, _) => ("import polars as pl", "pl.DataFrame(data)".to_string()),
        };
        return format!("import json\n{import}\n\nwith open({path}) as f:\n    data = json.load(f)\ndf = {build}\ndf");
    }
    let sep = |default: u8| sniffed.separator.filter(|&s| s != default).map(|s| py_str(&(s as char).to_string()));
    let table = sniffed.table.as_deref().map(py_str);
    let (import, load) = match library {
        Library::Pandas => (
            "import pandas as pd",
            match format {
                Format::Csv { .. } => {
                    let mut args = vec![path.clone()];
                    args.extend(sep(b',').map(|s| format!("sep={s}")));
                    if sniffed.decimal_comma {
                        args.push("decimal=\",\"".into());
                    }
                    format!("pd.read_csv({})", args.join(", "))
                }
                Format::Tsv { .. } => format!("pd.read_csv({path}, sep=\"\\t\")"),
                Format::Parquet => format!("pd.read_parquet({path})"),
                Format::Arrow => format!("pd.read_feather({path})"),
                Format::Json => format!("pd.read_json({path})"),
                Format::JsonLines => format!("pd.read_json({path}, lines=True)"),
                Format::Excel => match &table {
                    Some(t) => format!("pd.read_excel({path}, sheet_name={t})"),
                    None => format!("pd.read_excel({path})"),
                },
                Format::Sqlite => {
                    let t = sniffed.table.as_deref().unwrap_or("table");
                    return format!(
                        "import sqlite3\nimport pandas as pd\n\ncon = sqlite3.connect({path})\ndf = pd.read_sql({}, con)\ndf",
                        py_str(&format!("SELECT * FROM \"{}\"", t.replace('"', "\"\"")))
                    );
                }
            },
        ),
        Library::Polars => (
            "import polars as pl",
            match format {
                Format::Csv { .. } => {
                    let mut args = vec![path.clone()];
                    args.extend(sep(b',').map(|s| format!("separator={s}")));
                    if sniffed.decimal_comma {
                        args.push("decimal_comma=True".into());
                    }
                    format!("pl.read_csv({})", args.join(", "))
                }
                Format::Tsv { .. } => format!("pl.read_csv({path}, separator=\"\\t\")"),
                Format::Parquet => format!("pl.read_parquet({path})"),
                Format::Arrow => format!("pl.read_ipc({path})"),
                Format::Json => format!("pl.read_json({path})"),
                Format::JsonLines => format!("pl.read_ndjson({path})"),
                Format::Excel => match &table {
                    Some(t) => format!("pl.read_excel({path}, sheet_name={t})"),
                    None => format!("pl.read_excel({path})"),
                },
                Format::Sqlite => {
                    let t = sniffed.table.as_deref().unwrap_or("table");
                    return format!(
                        "import sqlite3\nimport polars as pl\n\ncon = sqlite3.connect({path})\ndf = pl.read_database({}, connection=con)\ndf",
                        py_str(&format!("SELECT * FROM \"{}\"", t.replace('"', "\"\"")))
                    );
                }
            },
        ),
    };
    format!("{import}\n\ndf = {load}\ndf")
}

/// A Python string literal.
fn py_str(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cells() {
        let none = Sniffed::default();
        assert_eq!(
            loading_code("sales.csv", Format::Csv { gz: false }, &none, Library::Pandas),
            "import pandas as pd\n\ndf = pd.read_csv(\"sales.csv\")\ndf"
        );
        let semi = Sniffed { separator: Some(b';'), ..Default::default() };
        assert_eq!(
            loading_code("eu.csv", Format::Csv { gz: false }, &semi, Library::Polars),
            "import polars as pl\n\ndf = pl.read_csv(\"eu.csv\", separator=\";\")\ndf"
        );
        let sheet = Sniffed { table: Some("Q3".into()), ..Default::default() };
        assert!(loading_code("b.xlsx", Format::Excel, &sheet, Library::Pandas).contains("sheet_name=\"Q3\""));
        let t = Sniffed { table: Some("users".into()), ..Default::default() };
        let eu = Sniffed { separator: Some(b';'), decimal_comma: true, ..Default::default() };
        assert_eq!(
            loading_code("eu.csv", Format::Csv { gz: false }, &eu, Library::Pandas),
            "import pandas as pd\n\ndf = pd.read_csv(\"eu.csv\", sep=\";\", decimal=\",\")\ndf"
        );
        assert!(loading_code("eu.csv", Format::Csv { gz: false }, &eu, Library::Polars).contains("separator=\";\", decimal_comma=True"));
        assert!(loading_code("app.db", Format::Sqlite, &t, Library::Pandas).contains("pd.read_sql(\"SELECT * FROM \\\"users\\\"\", con)"));
        assert!(loading_code("a \"b\".json", Format::Json, &none, Library::Pandas).contains("read_json(\"a \\\"b\\\".json\")"));
    }
}
