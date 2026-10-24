# ---
# jupyter:
#   jupytext:
#     formats: ipynb,py:percent
#     text_representation:
#       extension: .py
#       format_name: percent
#       format_version: '1.3'
#       jupytext_version: 1.16.4
#   kernelspec:
#     display_name: Python 3
#     language: python
#     name: python3
# ---

# %% [markdown]
# # Jupytext paired notebook
#
# Paired with `jupytext_paired.ipynb` (percent format).  Outputs live only in the `.ipynb`.

# %%
import numpy as np
import pandas as pd
# %%
def square(x):
    return x * x


square(7)

# %% tags=["parameters"] title="Parameters"
alpha = 0.5
