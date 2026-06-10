import importlib

__version__ = "0.3.0"

__all__ = [
    "CrystalXMLSource",
    "Pipeline",
    "RenameFields",
    "CastTypes",
    "FilterRows",
    "DropFields",
    "to_dataframe",
    "to_csv",
    "collect",
]

_modules = {
    "CrystalXMLSource": ".source",
    "Pipeline": ".pipeline",
    "RenameFields": ".stages",
    "CastTypes": ".stages",
    "FilterRows": ".stages",
    "DropFields": ".stages",
    "to_dataframe": ".sinks",
    "to_csv": ".sinks",
    "collect": ".sinks",
}

def __getattr__(name):
    if name in _modules:
        mod = importlib.import_module(_modules[name], __package__)
        return getattr(mod, name)
    raise AttributeError(f"module {__name__!r} has no attribute {name!r}")

def __dir__():
    return __all__