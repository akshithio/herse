"""Generates the sample charts the herse-plot tests and gallery render.

    /opt/anaconda3/bin/python3 crates/plot/tests/data/gen.py

Writes `plotly/<name>.json` (the value of an `application/vnd.plotly.v1+json`
bundle: `{"data", "layout"}`) and `vegalite/<name>.json` (an Altair
`chart.to_dict()`, i.e. what `application/vnd.vegalite.v5+json` holds) next
to this script. Uses only offline data (plotly's bundled datasets + numpy).
Seeds are fixed, so re-running produces the same files.
"""

import json
import os

import numpy as np
import pandas as pd
import plotly.express as px
import plotly.graph_objects as go
import plotly.io as pio
from plotly.subplots import make_subplots
import altair as alt

HERE = os.path.dirname(os.path.abspath(__file__))
PLOTLY = os.path.join(HERE, "plotly")
VEGA = os.path.join(HERE, "vegalite")
os.makedirs(PLOTLY, exist_ok=True)
os.makedirs(VEGA, exist_ok=True)

alt.data_transformers.disable_max_rows()


def save_plotly(name, fig):
    # Same JSON plotly writes into the notebook's mime bundle.
    d = json.loads(pio.to_json(fig, validate=False))
    with open(os.path.join(PLOTLY, name + ".json"), "w") as f:
        json.dump({"data": d["data"], "layout": d["layout"]}, f)


def save_vl(name, chart):
    with open(os.path.join(VEGA, name + ".json"), "w") as f:
        json.dump(chart.to_dict(), f)


rng = np.random.default_rng(7)
tips = px.data.tips()
iris = px.data.iris()
gap = px.data.gapminder()
gap07 = gap.query("year == 2007")
stocks = px.data.stocks()

# ---------------------------------------------------------------- plotly
x = np.linspace(0, 10, 60)
save_plotly("scatter_markers", go.Figure(go.Scatter(x=rng.normal(size=80), y=rng.normal(size=80), mode="markers")))
save_plotly("scatter_lines", go.Figure(go.Scatter(x=x, y=np.sin(x), mode="lines", name="sin")))
fig = go.Figure()
fig.add_trace(go.Scatter(x=x[::4], y=np.sin(x[::4]), mode="lines+markers", name="sin"))
fig.add_trace(go.Scatter(x=x[::4], y=np.cos(x[::4]), mode="lines+markers", name="cos"))
fig.add_trace(go.Scatter(x=x[::4], y=0.5 * np.sin(2 * x[::4]), mode="lines", name="half", line=dict(dash="dash")))
fig.update_layout(title="Multiple traces", xaxis_title="x", yaxis_title="value")
save_plotly("scatter_multi", fig)
save_plotly("scatter_color_group", px.scatter(iris, x="sepal_width", y="sepal_length", color="species", title="Iris"))
save_plotly(
    "scatter_size_logx",
    px.scatter(gap07, x="gdpPercap", y="lifeExp", size="pop", color="continent", hover_name="country", log_x=True, size_max=60, title="Gapminder 2007"),
)
save_plotly("scatter_continuous_color", px.scatter(iris, x="sepal_width", y="sepal_length", color="petal_length"))
save_plotly("scatter_symbols", px.scatter(tips, x="total_bill", y="tip", color="sex", symbol="smoker"))
save_plotly("scatter_categorical_x", go.Figure(go.Scatter(x=["Mon", "Tue", "Wed", "Thu", "Fri"], y=[3, 5, 2, 8, 6], mode="lines+markers")))
save_plotly("scatter_error_bars", px.scatter(x=[1, 2, 3, 4, 5], y=[2, 3.5, 3, 5, 4.4], error_y=[0.3, 0.5, 0.2, 0.6, 0.4]))
save_plotly("line_px_countries", px.line(gap.query("continent == 'Oceania'"), x="year", y="lifeExp", color="country", markers=True))
save_plotly("line_dates", px.line(stocks, x="date", y=["GOOG", "AAPL", "AMZN"], title="Stocks"))
hours = pd.date_range("2024-03-01", periods=72, freq="h")
save_plotly("line_dates_hourly", go.Figure(go.Scatter(x=hours, y=np.cumsum(rng.normal(size=72)), mode="lines")))
days = pd.date_range("2023-01-01", periods=730, freq="D")
save_plotly("line_dates_years", go.Figure(go.Scatter(x=days, y=np.cumsum(rng.normal(size=730)), mode="lines")))
save_plotly("line_shapes", go.Figure([
    go.Scatter(x=[1, 2, 3, 4, 5], y=[1, 3, 2, 3, 1], mode="lines+markers", name="linear"),
    go.Scatter(x=[1, 2, 3, 4, 5], y=[2, 4, 3, 4, 2], mode="lines+markers", name="spline", line_shape="spline"),
    go.Scatter(x=[1, 2, 3, 4, 5], y=[3, 5, 4, 5, 3], mode="lines+markers", name="hv", line_shape="hv"),
]))

save_plotly("bar_simple", px.bar(x=["a", "b", "c", "d", "e"], y=[12, 18, 7, 22, 15], title="Simple bar"))
animals = ["giraffes", "orangutans", "monkeys"]
fig = go.Figure([go.Bar(name="SF Zoo", x=animals, y=[20, 14, 23]), go.Bar(name="LA Zoo", x=animals, y=[12, 18, 29])])
fig.update_layout(barmode="group", title="Grouped bars")
save_plotly("bar_grouped", fig)
save_plotly("bar_stacked", px.bar(tips, x="day", y="total_bill", color="sex", title="Stacked (px default)"))
save_plotly("bar_horizontal", px.bar(x=[30, 12, 45, 22], y=["alpha", "beta", "gamma", "delta"], orientation="h"))
save_plotly("bar_negative", go.Figure(go.Bar(x=list(range(12)), y=np.round(rng.normal(size=12) * 1000, 1))))
save_plotly("bar_text_auto", px.bar(x=["A", "B", "C"], y=[1.25, 3.5, 2.75], text_auto=True))
save_plotly("bar_large_values", px.bar(x=["2019", "2020", "2021", "2022"], y=[1.2e6, 2.6e6, 3.1e6, 4.8e6], title="Revenue"))

save_plotly("hist_raw", px.histogram(tips, x="total_bill", title="Histogram"))
save_plotly("hist_nbins_color", px.histogram(tips, x="total_bill", color="sex", nbins=20))
fig = go.Figure([go.Histogram(x=rng.normal(size=500), name="a"), go.Histogram(x=rng.normal(1, size=500), name="b")])
fig.update_layout(barmode="overlay")
fig.update_traces(opacity=0.75)
save_plotly("hist_overlay", fig)
save_plotly("hist_normal_go", go.Figure(go.Histogram(x=rng.normal(size=2000))))
save_plotly("hist_percent_h", px.histogram(tips, y="tip", histnorm="percent"))
save_plotly("hist_categorical", px.histogram(tips, x="day"))

save_plotly("box", px.box(tips, x="day", y="total_bill", title="Box"))
save_plotly("box_grouped", px.box(tips, x="day", y="total_bill", color="smoker"))
save_plotly("box_points_all", go.Figure(go.Box(y=rng.normal(size=60), boxpoints="all", name="sample")))

z = np.round(rng.random((6, 8)) * 10, 1)
save_plotly("heatmap", go.Figure(go.Heatmap(z=z, x=[f"c{i}" for i in range(8)], y=[f"r{i}" for i in range(6)])))
save_plotly("heatmap_numeric", go.Figure(go.Heatmap(z=np.sin(np.add.outer(np.linspace(0, 3, 30), np.linspace(0, 4, 40))))))
save_plotly("imshow_text", px.imshow(np.round(np.corrcoef(rng.normal(size=(5, 30))), 2), text_auto=True, x=list("ABCDE"), y=list("ABCDE")))

save_plotly("pie", px.pie(tips, values="tip", names="day", title="Tips by day"))
save_plotly("donut", go.Figure(go.Pie(labels=["Oxygen", "Hydrogen", "Carbon_Dioxide", "Nitrogen"], values=[4500, 2500, 1053, 500], hole=0.4)))

save_plotly("area_tozeroy", go.Figure(go.Scatter(x=x, y=np.sin(x) + 1.2, fill="tozeroy")))
area_df = pd.DataFrame({"year": np.repeat(np.arange(2000, 2011), 3), "kind": ["a", "b", "c"] * 11, "v": rng.integers(1, 10, 33)})
save_plotly("area_px_stacked", px.area(area_df, x="year", y="v", color="kind"))

save_plotly("log_y", go.Figure(go.Scatter(x=np.arange(1, 30), y=np.exp(np.arange(1, 30) * 0.5), mode="lines+markers"), layout=dict(yaxis_type="log")))

fig = make_subplots(rows=1, cols=2, subplot_titles=("left", "right"))
fig.add_trace(go.Scatter(x=[1, 2, 3], y=[4, 5, 6]), row=1, col=1)
fig.add_trace(go.Bar(x=["a", "b", "c"], y=[3, 1, 2]), row=1, col=2)
fig.update_layout(title="make_subplots 1x2", showlegend=False)
save_plotly("subplots_1x2", fig)
fig = make_subplots(rows=2, cols=2)
fig.add_trace(go.Scatter(x=x, y=np.sin(x)), row=1, col=1)
fig.add_trace(go.Scatter(x=x, y=np.cos(x)), row=1, col=2)
fig.add_trace(go.Bar(x=list("abcd"), y=[2, 5, 3, 4]), row=2, col=1)
fig.add_trace(go.Histogram(x=rng.normal(size=300)), row=2, col=2)
save_plotly("subplots_2x2", fig)
fig = make_subplots(specs=[[{"secondary_y": True}]])
fig.add_trace(go.Scatter(x=[1, 2, 3, 4], y=[40, 50, 60, 55], name="left"), secondary_y=False)
fig.add_trace(go.Scatter(x=[1, 2, 3, 4], y=[4, 5, 6, 8], name="right"), secondary_y=True)
save_plotly("secondary_y", fig)
save_plotly("facet_col", px.scatter(tips, x="total_bill", y="tip", color="smoker", facet_col="sex"))
save_plotly("facet_grid", px.histogram(tips, x="total_bill", facet_row="time", facet_col="day"))

fig = px.scatter(iris, x="sepal_width", y="sepal_length", template="plotly_white", title="plotly_white")
fig.add_hline(y=6, line_dash="dot", annotation_text="y = 6")
fig.add_vline(x=3, line_color="red")
save_plotly("white_shapes", fig)
save_plotly("dark_template", px.line(gap.query("country in ['Canada','Japan','Brazil']"), x="year", y="gdpPercap", color="country", template="plotly_dark"))
save_plotly("ggplot_template", px.scatter(iris, x="petal_width", y="petal_length", color="species", template="ggplot2"))
save_plotly("legend_horizontal", px.line(stocks, x="date", y=["GOOG", "AAPL"]).update_layout(legend=dict(orientation="h", yanchor="bottom", y=1.02, xanchor="right", x=1)))
save_plotly("ecdf", px.ecdf(tips, x="total_bill", color="sex"))
save_plotly("animation_first_frame", px.scatter(gap, x="gdpPercap", y="lifeExp", animation_frame="year", size="pop", color="continent", log_x=True, size_max=45, range_x=[100, 100000], range_y=[25, 90]))

save_plotly("density_heatmap", px.density_heatmap(tips, x="total_bill", y="tip"))
img = (np.dstack([np.add.outer(np.arange(40), np.zeros(60)) * 6, np.add.outer(np.zeros(40), np.arange(60)) * 4, np.full((40, 60), 120)])).astype(np.uint8)
save_plotly("imshow_rgb", px.imshow(img))
save_plotly("image_go", go.Figure(go.Image(z=img[::4, ::4])))
save_plotly("ohlc", go.Figure(go.Ohlc(x=stocks.date[:40], open=stocks.AAPL[:40], high=stocks.AAPL[:40] * 1.03, low=stocks.AAPL[:40] * 0.97, close=stocks.AAPL[1:41].values), layout=dict(xaxis_rangeslider_visible=False)))
# Big data (100k points) is synthesized by the Rust tests / gallery instead of
# being stored here.

# Unsupported -> None.
save_plotly("unsupported_violin", px.violin(tips, y="tip"))
save_plotly("unsupported_scatter3d", px.scatter_3d(iris, x="sepal_length", y="sepal_width", z="petal_width"))
save_plotly("candlestick", go.Figure(go.Candlestick(x=stocks.date[:30], open=stocks.GOOG[:30], high=stocks.GOOG[:30] * 1.02, low=stocks.GOOG[:30] * 0.98, close=stocks.GOOG[:30] * 1.01)))

# ---------------------------------------------------------------- vega-lite
cars = pd.DataFrame({
    "hp": rng.integers(60, 250, 150),
    "mpg": rng.normal(25, 6, 150).round(1),
    "weight": rng.normal(3000, 600, 150).round(0),
    "origin": rng.choice(["USA", "Europe", "Japan"], 150),
    "cyl": rng.choice([4, 6, 8], 150),
})
save_vl("point_color", alt.Chart(cars).mark_point().encode(x="hp", y="mpg", color="origin").properties(title="Cars"))
save_vl("circle_size", alt.Chart(cars).mark_circle().encode(x="hp", y="weight", size="mpg", color="origin"))
save_vl("point_quant_color", alt.Chart(cars).mark_circle(size=60).encode(x="hp:Q", y="mpg:Q", color="weight:Q"))
save_vl("point_shape", alt.Chart(cars).mark_point().encode(x="hp", y="mpg", shape="origin", color="origin"))
save_vl("bar_simple", alt.Chart(pd.DataFrame({"a": list("ABCDEFG"), "b": [28, 55, 43, 91, 81, 53, 19]})).mark_bar().encode(x="a", y="b"))
save_vl("bar_count", alt.Chart(cars).mark_bar().encode(x="origin:N", y="count()"))
save_vl("bar_mean_horizontal", alt.Chart(cars).mark_bar().encode(y="origin:N", x="mean(mpg):Q"))
save_vl("bar_stacked", alt.Chart(cars).mark_bar().encode(x="cyl:O", y="count()", color="origin"))
save_vl("bar_grouped_xoffset", alt.Chart(cars).mark_bar().encode(x="cyl:O", xOffset="origin:N", y="mean(hp):Q", color="origin:N"))
save_vl("bar_sorted", alt.Chart(cars).mark_bar().encode(x=alt.X("origin:N", sort="-y"), y="sum(weight):Q"))
save_vl("histogram", alt.Chart(cars).mark_bar().encode(x=alt.X("mpg:Q", bin=True), y="count()"))
save_vl("histogram_maxbins", alt.Chart(cars).mark_bar().encode(x=alt.X("weight:Q", bin=alt.Bin(maxbins=30)), y="count()", color="origin"))
ts = pd.DataFrame({"date": pd.date_range("2022-01-01", periods=365, freq="D")})
ts["value"] = np.cumsum(rng.normal(size=365)).round(2)
ts["series"] = "a"
ts2 = ts.copy()
ts2["value"] = np.cumsum(rng.normal(size=365)).round(2)
ts2["series"] = "b"
ts_all = pd.concat([ts, ts2])
save_vl("line_temporal", alt.Chart(ts_all).mark_line().encode(x="date:T", y="value:Q", color="series:N"))
save_vl("line_timeunit_month", alt.Chart(ts).mark_line(point=True).encode(x="yearmonth(date):T", y="mean(value):Q"))
save_vl("bar_timeunit_month", alt.Chart(ts).mark_bar().encode(x="month(date):O", y="mean(value):Q"))
save_vl("area_stacked", alt.Chart(area_df).mark_area().encode(x="year:Q", y="v:Q", color="kind:N"))
save_vl("area_simple", alt.Chart(ts).mark_area(opacity=0.6).encode(x="date:T", y="value:Q"))
hm = pd.DataFrame([(i, j, float(np.sin(i / 2) * np.cos(j / 3))) for i in range(10) for j in range(8)], columns=["x", "y", "z"])
save_vl("rect_heatmap", alt.Chart(hm).mark_rect().encode(x="x:O", y="y:O", color="z:Q"))
save_vl("layer_line_point", alt.Chart(ts.iloc[:40]).mark_line().encode(x="date:T", y="value:Q") + alt.Chart(ts.iloc[:40]).mark_point().encode(x="date:T", y="value:Q"))
bars = alt.Chart(cars).mark_bar().encode(x="origin:N", y="mean(mpg):Q")
rule = alt.Chart(cars).mark_rule(color="red").encode(y="mean(mpg):Q")
save_vl("layer_bar_rule", (bars + rule).properties(width=300))
text = bars.mark_text(dy=-6).encode(text=alt.Text("mean(mpg):Q", format=".1f"))
save_vl("layer_bar_text", bars + text)
save_vl("facet_column", alt.Chart(cars).mark_point().encode(x="hp", y="mpg", color="origin").properties(width=180, height=180).facet(column="origin"))
save_vl("facet_row_encoding", alt.Chart(cars).mark_bar().encode(x="count()", y="cyl:O", row="origin:N"))
save_vl("interactive_scatter", alt.Chart(cars).mark_circle().encode(x="hp", y="mpg", color="origin", tooltip=["hp", "mpg"]).interactive())
sel = alt.selection_point(fields=["origin"], bind="legend")
save_vl("selection_legend", alt.Chart(cars).mark_point().encode(x="hp", y="mpg", color="origin", opacity=alt.condition(sel, alt.value(1), alt.value(0.2))).add_params(sel))
save_vl("filter_calculate", alt.Chart(cars).transform_filter(alt.datum.hp > 100).transform_calculate(kpl="datum.mpg * 0.425").mark_point().encode(x="hp", y="kpl:Q"))
save_vl("log_scale", alt.Chart(pd.DataFrame({"x": np.arange(1, 40), "y": np.exp(np.arange(1, 40) * 0.3)})).mark_line().encode(x="x", y=alt.Y("y", scale=alt.Scale(type="log"))))
save_vl("arc_pie", alt.Chart(pd.DataFrame({"category": list("abcdef"), "value": [4, 6, 10, 3, 7, 8]})).mark_arc().encode(theta="value:Q", color="category:N"))
save_vl("custom_domain_scheme", alt.Chart(cars).mark_point(filled=True).encode(x=alt.X("hp", scale=alt.Scale(domain=[50, 260])), y=alt.Y("mpg", scale=alt.Scale(zero=False)), color=alt.Color("origin", scale=alt.Scale(scheme="dark2"))).properties(width=400, height=250))
save_vl("hconcat", alt.hconcat(alt.Chart(cars).mark_point().encode(x="hp", y="mpg"), alt.Chart(cars).mark_bar().encode(x="origin", y="count()")))
save_vl("unsupported_url_data", alt.Chart("https://example.com/data.csv").mark_point().encode(x="a:Q", y="b:Q"))


# ---------------------------------------------------------------- more plotly edge cases
cats = [f"category number {i}" for i in range(14)]
save_plotly("bar_long_categories", px.bar(x=cats, y=rng.integers(1, 20, 14)))
fig = px.scatter(x=[1, 2, 3, 4], y=[10, 11, 12, 13], text=["a", "b", "c", "d"])
fig.update_traces(textposition="top center")
fig.add_annotation(x=2, y=11, text="annotated point", showarrow=True, arrowhead=1)
fig.add_annotation(x=0.5, y=1.08, xref="paper", yref="paper", text="<b>bold</b> paper note", showarrow=False)
save_plotly("annotations_text", fig)
fig = go.Figure(go.Scatter(x=[0, 1, 2, 3], y=[0.1, 0.25, 0.4, 0.8], mode="lines+markers"))
fig.update_layout(yaxis_tickformat=".0%", xaxis_ticksuffix=" s", xaxis_range=[-0.5, 3.5], yaxis_autorange="reversed", title={"text": "Formats, ranges", "x": 0.5})
save_plotly("tickformat_reversed", fig)
fig = px.line(stocks, x="date", y="GOOG", markers=True, line_shape="spline")
fig.update_layout(legend=dict(orientation="h"), showlegend=True, xaxis=dict(showline=True, linecolor="black", ticks="outside", mirror=True), yaxis=dict(showline=True, linecolor="black", ticks="outside", mirror=True, showgrid=False))
save_plotly("axis_lines_mirror", fig)
save_plotly("hist_xbins", go.Figure(go.Histogram(x=tips.total_bill, xbins=dict(start=0, end=60, size=5), marker_line=dict(color="white", width=1))))
save_plotly("bar_dates", px.bar(x=pd.date_range("2024-01-01", periods=12, freq="MS"), y=rng.integers(5, 30, 12)))
save_plotly("scatter_white_bubble", px.scatter(gap07, x="gdpPercap", y="lifeExp", size="pop", color="continent", log_x=True, template="plotly_white"))
fig = go.Figure(go.Bar(x=["a", "b", "c"], y=[1, 3, 2]))
fig.update_layout(template="none", title="no template")
save_plotly("no_template", fig)
fig = make_subplots(rows=2, cols=1, shared_xaxes=True, vertical_spacing=0.05)
fig.add_trace(go.Scatter(x=x, y=np.sin(x), name="top"), row=1, col=1)
fig.add_trace(go.Bar(x=x[::5], y=np.cos(x[::5]), name="bottom"), row=2, col=1)
save_plotly("subplots_shared_x", fig)
save_plotly("scatter_category_both", px.scatter(tips, x="day", y="time", size="size", color="sex"))
save_plotly("box_horizontal", px.box(tips, x="total_bill", y="day", orientation="h"))
fig = go.Figure(go.Heatmap(z=rng.normal(size=(60, 80)), colorscale="RdBu", zmid=0, colorbar=dict(title="z")))
save_plotly("heatmap_large_diverging", fig)
save_plotly("pie_labels", px.pie(values=[30, 25, 15, 10, 8, 5, 4, 3], names=list("ABCDEFGH")).update_traces(textinfo="label+percent"))

# ---------------------------------------------------------------- more vega-lite
save_vl("boxplot", alt.Chart(cars).mark_boxplot().encode(x="origin:N", y="mpg:Q"))
save_vl("strip_tick", alt.Chart(cars).mark_tick().encode(x="hp:Q", y="origin:N"))
save_vl("text_labels", alt.Chart(cars.head(20)).mark_text(align="left", dx=4).encode(x="hp", y="mpg", text="origin") + alt.Chart(cars.head(20)).mark_point().encode(x="hp", y="mpg"))
save_vl("stack_normalize", alt.Chart(cars).mark_bar().encode(x="cyl:O", y=alt.Y("count()", stack="normalize"), color="origin"))
save_vl("donut_text", alt.Chart(pd.DataFrame({"k": list("abcd"), "v": [5, 3, 2, 1]})).mark_arc(innerRadius=50).encode(theta="v:Q", color="k:N"))
save_vl("vconcat", alt.vconcat(alt.Chart(cars).mark_bar().encode(x="hp:Q", y="count()").properties(height=100), alt.Chart(cars).mark_circle().encode(x="hp", y="mpg").properties(height=150)))
save_vl("concat_wrap", alt.concat(*[alt.Chart(cars).mark_bar().encode(x=alt.X(f, bin=True), y="count()").properties(width=150, height=100) for f in ["hp", "mpg", "weight"]], columns=2))
save_vl("facet_row_column", alt.Chart(cars).mark_point().encode(x="hp", y="mpg").properties(width=120, height=100).facet(row="cyl:O", column="origin:N"))
save_vl("window_cumsum", alt.Chart(ts.iloc[:60]).transform_window(cum="sum(value)").mark_line().encode(x="date:T", y="cum:Q"))
save_vl("fold_lines", alt.Chart(pd.DataFrame({"t": np.arange(30), "a": np.sin(np.arange(30) / 4), "b": np.cos(np.arange(30) / 4)})).transform_fold(["a", "b"]).mark_line(interpolate="monotone").encode(x="t:Q", y="value:Q", color="key:N", strokeDash="key:N"))
save_vl("ordinal_color_bar", alt.Chart(cars).mark_bar().encode(x="origin:N", y="mean(hp):Q", color="cyl:O"))
hours = pd.DataFrame({"t": pd.date_range("2024-05-01", periods=96, freq="h"), "v": rng.normal(size=96).cumsum()})
save_vl("hours_timeunit", alt.Chart(hours).mark_bar().encode(x="hours(t):O", y="mean(v):Q"))
save_vl("temporal_bar", alt.Chart(ts).mark_bar().encode(x="yearmonth(date):T", y="sum(value):Q"))
save_vl("dark_theme_config", alt.Chart(cars).mark_point().encode(x="hp", y="mpg", color="origin").configure(background="#333", axis={"labelColor": "#eee", "titleColor": "#eee", "gridColor": "#555", "domainColor": "#aaa"}, legend={"labelColor": "#eee", "titleColor": "#eee"}))
save_vl("rule_band", alt.Chart(cars).mark_rule().encode(x="min(hp):Q", x2="max(hp):Q", y="origin:N") + alt.Chart(cars).mark_point(filled=True).encode(x="mean(hp):Q", y="origin:N"))
save_vl("nb_like_facet_hist", alt.Chart(cars).mark_bar().encode(x=alt.X("mpg:Q", bin=alt.Bin(maxbins=20)), y="count()", color="origin").facet("origin:N", columns=3))
save_vl("sort_count_bars", alt.Chart(cars).mark_bar().encode(y=alt.Y("origin:N", sort="-x"), x="count()"))

print("wrote", len(os.listdir(PLOTLY)), "plotly and", len(os.listdir(VEGA)), "vega-lite samples")
