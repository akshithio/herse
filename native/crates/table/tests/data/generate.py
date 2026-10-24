# Regenerates the sample HTML outputs in this directory (pandas 2.2, polars 1.33, xarray).
# Usage: /opt/anaconda3/bin/python3 generate.py <out_dir>   (the checked-in files are a subset)
import sys, os
import numpy as np, pandas as pd
out = sys.argv[1]
def w(name, html):
    open(os.path.join(out, name + ".html"), "w").write(html)
rng = np.random.default_rng(0)
df = pd.DataFrame(rng.normal(size=(6, 4)).round(3), columns=list("abcd"))
# Styler with table styles + hide index + format
st = (df.style.set_table_styles([
        {"selector": "th", "props": [("background-color", "rgb(30, 41, 59)"), ("color", "white")]},
        {"selector": "td:hover", "props": "background-color: yellow;"},
        {"selector": ".col_heading", "props": "font-style: italic;"},
        {"selector": "caption", "props": "caption-side: bottom; font-size: 1.2em;"},
      ]).hide(axis="index").format("{:.2f}").set_caption("Hidden index &amp; formatted"))
w("styler_table_styles_hide_index", st.to_html())
w("styler_repr_html", st._repr_html_())
# bar
st = df.style.bar(subset=["a", "b"], color=["#d65f5f", "#5fba7d"], align="mid").bar(subset=["c"], color="#5fba7d")
w("styler_bar", st._repr_html_())
# multiindex columns with colspan via styler
cols = pd.MultiIndex.from_product([["x", "y"], ["mean", "std"]])
mi = pd.DataFrame(rng.random((4, 4)).round(2), columns=cols, index=pd.MultiIndex.from_product([["g1","g2"],["a","b"]], names=["grp","sub"]))
w("styler_multiindex", mi.style.background_gradient(cmap="Blues")._repr_html_())
w("df_multiindex", mi._repr_html_())
# set_properties and applymap with inline-ish props, font-weight
st = df.style.set_properties(**{"color": "rgba(255, 0, 0, 0.5)", "font-weight": "bold"}, subset=["a"]).map(lambda v: "font-style: italic; background: lightblue" if v > 0 else None, subset=["b"])
w("styler_props", st._repr_html_())
# highlight_null, text_gradient
dfn = df.copy(); dfn.iloc[1, 2] = np.nan
w("styler_null_textgrad", dfn.style.highlight_null(color="red").text_gradient(cmap="viridis", subset=["a"])._repr_html_())
# Styler truncation (max rows / cols)
big = pd.DataFrame(rng.random((300, 30)))
with pd.option_context("styler.render.max_rows", 20, "styler.render.max_columns", 8):
    w("styler_truncated", big.style.format("{:.3f}")._repr_html_())
# Pandas long and wide with truncation both axes
with pd.option_context("display.max_rows", 10, "display.max_columns", 6):
    w("df_trunc_both", big._repr_html_())
# Series to_frame, describe
s = pd.Series(rng.random(5), name="vals")
w("series_frame", s.to_frame()._repr_html_())
# df.to_html
w("df_to_html", df.to_html())
w("df_to_html_noindex", df.to_html(index=False))
# notebook=True style
w("df_to_html_nb", df.to_html(notebook=True))
# string, bool, categories, html escapes, unicode, long text, newlines
weird = pd.DataFrame({"txt": ["a & b", "<i>x</i>", "line1\nline2", "  spaced   out  ", "日本語テキスト", "é"], "pct": ["12%", "-3.5%", "1e-5", "1,234", "$5", "−7"], "n": [1, None, 3, 4, 5, 6]})
w("df_weird", weird._repr_html_())
# Styler with hide columns, to_html with doctype?
w("styler_doctype", df.style.to_html(doctype_html=True))
w("styler_hide_cols_index_names", mi.style.hide(axis="columns", level=1)._repr_html_())
# timedelta, datetime tz, categorical
t = pd.DataFrame({"td": pd.to_timedelta([1, 2, None], unit="h"), "dt": pd.date_range("2024", periods=3, tz="UTC"), "cat": pd.Categorical(["a", "b", None])})
w("df_time", t._repr_html_())
# Polars
try:
    import polars as pl
    p = pl.DataFrame({"a": [1, 2, 3, None, 5], "b": ["x", "y", None, "w", "v"], "c": [1.5, None, 3.25, 4.0, float("nan")], "d": [True, False, None, True, True]})
    w("polars_basic", p._repr_html_())
    pbig = pl.DataFrame({f"c{i}": list(range(100)) for i in range(30)})
    w("polars_trunc", pbig._repr_html_())
    w("polars_nested", pl.DataFrame({"l": [[1, 2], [3]], "s": [{"x": 1}, {"x": 2}], "dt": [pl.datetime(2024,1,1) if False else None, None]}).with_columns(pl.col("l"))._repr_html_())
    w("polars_series", pl.Series("vals", [1.0, 2.5, None])._repr_html_() if hasattr(pl.Series, "_repr_html_") else "")
    import datetime
    w("polars_dates", pl.DataFrame({"d": [datetime.date(2024,1,1), datetime.date(2024,2,1)], "t": [datetime.datetime(2024,1,1,12), None], "s": ["a<b", "&amp;"]})._repr_html_())
except Exception as e:
    print("polars failed", e)
try:
    import xarray as xr
    ds = xr.Dataset({"t": (("x", "y"), rng.random((2, 3)))}, coords={"x": [1, 2]})
    w("xarray_dataset", ds._repr_html_())
except Exception as e:
    print("xarray failed", e)
