"""Builds gallery.ipynb: every ipywidgets control, tqdm bars and interact,
executed with nbclient so the notebook stores the widget state the way
JupyterLab's "Save Widget State Automatically" does.

    /opt/anaconda3/bin/python3 generate.py
"""
import nbformat
from nbclient import NotebookClient

cells = [
    "import ipywidgets as w\nfrom IPython.display import display",
    "display(w.IntSlider(value=7, min=0, max=10, description='Int'))\n"
    "display(w.FloatSlider(value=0.25, min=0, max=1, step=0.05, description='Float', readout_format='.1f'))\n"
    "display(w.FloatLogSlider(value=1000, base=10, min=0, max=5, description='Log'))\n"
    "display(w.IntRangeSlider(value=[2, 7], min=0, max=10, description='Range'))\n"
    "display(w.FloatRangeSlider(value=[0.5, 2.5], min=0, max=5, description='FRange'))\n"
    "display(w.SelectionSlider(options=['scrambled', 'sunny side up', 'poached'], value='sunny side up', description='Eggs'))\n"
    "display(w.SelectionRangeSlider(options=['Jan', 'Feb', 'Mar', 'Apr', 'May'], index=(1, 3), description='Months'))\n"
    "display(w.IntSlider(value=3, min=0, max=10, orientation='vertical', description='Vert'))",
    "for style in ['', 'info', 'success', 'warning', 'danger']:\n"
    "    display(w.IntProgress(value=6, min=0, max=10, bar_style=style, description=style or 'default'))\n"
    "display(w.FloatProgress(value=0.3, max=1, style={'bar_color': '#ff00ff'}, description='Magenta'))\n"
    "display(w.IntProgress(value=4, max=10, orientation='vertical'))",
    "display(w.Text(value='Hello', placeholder='Type', description='Text'))\n"
    "display(w.Text(placeholder='Placeholder only', description='Empty'))\n"
    "display(w.Textarea(value='line one\\nline two', description='Area'))\n"
    "display(w.Password(value='secret', description='Password'))\n"
    "display(w.IntText(value=42, description='IntText'))\n"
    "display(w.BoundedFloatText(value=7.5, min=0, max=10, step=0.1, description='Bounded'))\n"
    "display(w.Combobox(placeholder='Choose', options=['Paul', 'John'], description='Combo'))",
    "display(w.Dropdown(options=['1', '2', '3'], value='2', description='Number'))\n"
    "display(w.RadioButtons(options=['pepperoni', 'pineapple', 'anchovies'], value='pineapple', description='Topping'))\n"
    "display(w.Select(options=['Linux', 'Windows', 'macOS'], value='macOS', rows=3, description='OS'))\n"
    "display(w.SelectMultiple(options=['Apples', 'Oranges', 'Pears'], value=['Oranges', 'Pears'], description='Fruits'))\n"
    "display(w.ToggleButtons(options=['Slow', 'Regular', 'Fast'], value='Regular', description='Speed', button_style='info'))",
    "display(w.ToggleButton(value=True, description='Toggled', button_style='success', icon='check'))\n"
    "display(w.Valid(value=False, description='Valid'))\n"
    "display(w.Checkbox(value=True, description='Check me'))\n"
    "display(w.Checkbox(value=False, description='Unchecked', indent=False))\n"
    "display(w.HBox([w.Button(description=s or 'default', button_style=s) for s in ['', 'primary', 'success', 'info', 'warning', 'danger']]))\n"
    "b = w.Button(description='Custom'); b.style.button_color = 'lightgreen'; display(b)",
    "display(w.Label(value='A plain label'))\n"
    "display(w.HTML(value='<b>Bold</b> and <i>italic</i> <span style=\"color:red\">red</span>', description='HTML'))\n"
    "display(w.HTMLMath(value=r'Some math: $x^2 + y^2 = z^2$'))",
    "import io\nfrom PIL import Image\n"
    "buf = io.BytesIO(); Image.linear_gradient('L').resize((32, 32)).convert('RGB').save(buf, 'png')\n"
    "display(w.Image(value=buf.getvalue(), format='png', width=64, height=64))",
    "display(w.ColorPicker(concise=False, description='Color', value='#3366cc'))\n"
    "import datetime\n"
    "display(w.DatePicker(description='Date', value=datetime.date(2024, 3, 14)))\n"
    "display(w.Play(value=50, min=0, max=100, description='Play'))",
    "display(w.GridBox([w.Button(description=f'Cell {i}') for i in range(6)], layout=w.Layout(grid_template_columns='repeat(3, 120px)')))\n"
    "display(w.Stack([w.Label('first'), w.Label('second (selected)')], selected_index=1))\n"
    "t = w.Tab([w.Label('Tab one body'), w.Label('Tab two body')]); t.titles = ('One', 'Two'); t.selected_index = 1; display(t)\n"
    "a = w.Accordion([w.Label('Open body'), w.Label('Closed body')], titles=('Open', 'Closed')); a.selected_index = 0; display(a)\n"
    "display(w.VBox([w.Label('visible'), w.Label('hidden', layout=w.Layout(display='none'))]))",
    "from tqdm.notebook import tqdm\nimport time\nfor _ in tqdm(range(20), desc='Training'):\n    time.sleep(0.01)",
    "for _ in tqdm(range(100), desc='Stopped early', colour='green'):\n    if _ == 37: break",
    "from tqdm.auto import trange\nfor _ in trange(3, desc='auto'):\n    pass",
    "%matplotlib inline\nimport matplotlib\nmatplotlib.use('module://matplotlib_inline.backend_inline')\n"
    "import matplotlib.pyplot as plt\n"
    "@w.interact(n=(1, 10), label='curve')\n"
    "def f(n=4, label='curve'):\n"
    "    print(label, n)\n"
    "    plt.figure(figsize=(3, 2)); plt.plot([i ** n for i in range(5)]); plt.show()",
    "out = w.Output(layout={'border': '1px solid black'})\nwith out:\n    print('inside an Output widget')\n    display(w.Label('nested label'))\ndisplay(out)",
]

nb = nbformat.v4.new_notebook()
nb.cells = [nbformat.v4.new_code_cell(src) for src in cells]
nb.metadata["kernelspec"] = {"name": "python3", "display_name": "Python 3", "language": "python"}
NotebookClient(nb, timeout=120, kernel_name="python3", store_widget_state=True).execute()
nbformat.write(nb, "gallery.ipynb")
print("wrote gallery.ipynb")
