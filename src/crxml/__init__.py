import importlib

__version__ = "1.1.0"

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
    "XmlError",
    "PlanError",
    "MergeError",
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

_core_exceptions = {"XmlError", "PlanError", "MergeError"}

def __getattr__(name):
    if name in _core_exceptions:
        from crxml import _crxml_core as _core
        return getattr(_core, name)
    if name in _modules:
        mod = importlib.import_module(_modules[name], __package__)
        return getattr(mod, name)
    raise AttributeError(f"module {__name__!r} has no attribute {name!r}")

def __dir__():
    return __all__