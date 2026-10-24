"""Data files for herse-data's tests and the app's crash sweep.

    /opt/anaconda3/bin/python3 fixtures/data/make.py          # small files, here (checked in)
    /opt/anaconda3/bin/python3 fixtures/data/make.py --big    # plus ../big/data/: a 100 MB CSV and its Parquet

Needs numpy, pandas, pyarrow and openpyxl. Deterministic (fixed seed).
"""

import json
import os
import sqlite3
import sys

import numpy as np
import pandas as pd

HERE = os.path.dirname(os.path.abspath(__file__))


def sales(n, seed=7):
    rng = np.random.default_rng(seed)
    regions = np.array(["North", "South", "East", "West", "Ålesund", "São Paulo"])
    products = np.array(["Widget", "Gadget", "Doohickey", "Thingamajig", "Gizmo, large", 'The "Best" one'])
    df = pd.DataFrame(
        {
            "order_id": np.arange(1, n + 1),
            "date": pd.date_range("2024-01-01", periods=n, freq="37min").strftime("%Y-%m-%d %H:%M"),
            "region": regions[rng.integers(0, len(regions), n)],
            "product": products[rng.integers(0, len(products), n)],
            "units": rng.integers(1, 500, n),
            "price": np.round(rng.gamma(2.0, 20.0, n), 2),
            "discount": np.where(rng.random(n) < 0.15, np.nan, np.round(rng.random(n) * 0.3, 3)),
            "returned": rng.random(n) < 0.05,
            "rating": np.where(rng.random(n) < 0.1, None, rng.integers(1, 6, n).astype(object)),
            "note": np.where(rng.random(n) < 0.02, "call back\nnext week", ""),
        }
    )
    df["revenue"] = np.round(df.units * df.price * (1 - df.discount.fillna(0)), 2)
    return df


def small():
    df = sales(2000)
    df.to_csv(os.path.join(HERE, "sales.csv"), index=False)
    df.to_csv(os.path.join(HERE, "sales.tsv"), index=False, sep="\t")
    df.head(50).to_csv(os.path.join(HERE, "europe.csv"), index=False, sep=";", decimal=",")
    df.to_parquet(os.path.join(HERE, "sales.parquet"), index=False)
    df.set_index("order_id").head(300).to_parquet(os.path.join(HERE, "indexed.parquet"))
    df.reset_index(drop=True).to_feather(os.path.join(HERE, "sales.feather"))
    df.head(200).to_json(os.path.join(HERE, "records.json"), orient="records", indent=1)
    df.head(100).to_json(os.path.join(HERE, "columns.json"), orient="columns")
    df.head(100).to_json(os.path.join(HERE, "split.json"), orient="split")
    with open(os.path.join(HERE, "wrapped.json"), "w") as f:
        json.dump({"status": "ok", "count": 3, "results": [{"a": 1, "b": "x"}, {"a": 2, "c": [1, 2]}, {"a": None}]}, f)
    with open(os.path.join(HERE, "events.jsonl"), "w") as f:
        for i in range(500):
            ev = {"id": i, "kind": ["click", "view", "buy"][i % 3], "ms": i * 17 % 1000}
            if i % 50 == 0:
                ev["extra"] = {"campaign": f"c{i}"}  # a key that appears late
            f.write(json.dumps(ev) + "\n")
        f.write("{not json}\n")
    with pd.ExcelWriter(os.path.join(HERE, "book.xlsx")) as xl:
        df.head(300).to_excel(xl, sheet_name="Orders", index=False)
        df.groupby("region")[["units", "revenue"]].sum().reset_index().to_excel(xl, sheet_name="By region", index=False)
    path = os.path.join(HERE, "shop.sqlite")
    if os.path.exists(path):
        os.remove(path)
    con = sqlite3.connect(path)
    df.head(1000).to_sql("orders", con, index=False)
    pd.DataFrame({"region": ["North", "South"], "manager": ["Ada", "Grace"]}).to_sql("managers", con, index=False)
    con.execute("CREATE VIEW big_orders AS SELECT * FROM orders WHERE units > 400")
    con.commit()
    con.close()

    # Edge cases: each must open (or say why it can't) without a crash.
    edge = os.path.join(HERE, "edge")
    os.makedirs(edge, exist_ok=True)
    w = lambda name, data: open(os.path.join(edge, name), "wb").write(data)
    w("empty.csv", b"")
    w("header-only.csv", b"a,b,c\n")
    w("ragged.csv", b"a,b\n1,2\n3,4,5,6\n7\n")
    w("latin1.csv", "name,city\nJosé,Zürich\n".encode("latin-1"))
    w("bom.csv", b"\xef\xbb\xbfa,b\n1,2\n")
    w("quoted-newlines.csv", b'id,text\n1,"line one\nline two"\n2,"he said ""hi"""\n')
    w("unclosed-quote.csv", b'a,b\n1,"never closed\n2,3\n')
    w("utf16.csv", "a,b\n1,2\n".encode("utf-16"))
    w("not-parquet.parquet", b"PAR1 this is not a parquet file PAR1")
    w("truncated.parquet", open(os.path.join(HERE, "sales.parquet"), "rb").read()[:5000])
    w("broken.json", b'{"a": [1, 2, ')
    w("scalar.json", b"42")
    w("empty.jsonl", b"\n\n")
    w("not-a-workbook.xlsx", b"PK\x03\x04 nope")
    w("not-sqlite.sqlite", b"hello")
    w("wide.csv", (",".join(f"c{i}" for i in range(300)) + "\n" + ",".join(str(i) for i in range(300)) + "\n").encode())
    w("long-cell.csv", b"a,b\n" + b"x" * 200_000 + b",1\n")
    import gzip

    w("sales.csv.gz", gzip.compress(open(os.path.join(HERE, "sales.csv"), "rb").read()))


def big():
    out = os.path.join(HERE, "..", "big", "data")
    os.makedirs(out, exist_ok=True)
    df = sales(1_100_000, seed=11)
    df["comment"] = np.where(np.arange(len(df)) % 3 == 0, "a fairly long free-text comment field", "short")
    df.to_csv(os.path.join(out, "sales-1.1m.csv"), index=False)
    df.to_parquet(os.path.join(out, "sales-1.1m.parquet"), index=False)
    print({f: os.path.getsize(os.path.join(out, f)) for f in os.listdir(out)})


if __name__ == "__main__":
    small()
    if "--big" in sys.argv:
        big()
