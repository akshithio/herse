"""Helpers."""


def normalise(values):
    m = sum(values) / len(values)
    return [v - m for v in values]
