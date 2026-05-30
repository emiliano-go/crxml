from pathlib import Path
from typing import Iterator, Optional, Union
import io

try:
    from crxml._crxml_core import CrxmlReader
    _RUST = True
except ImportError:
    _RUST = False
    from lxml import etree

class CrystalXMLSource:
    """
    Streaming source for Crystal Reports XML.

    Args:
        source: file path (str or Path) or a file-like object (must have .name
            attribute or be a path). For file-like objects without a name,
            use the ``fileobj`` parameter.
        row_tag: XML tag that identifies data rows (default "Row").
        fileobj: if source is a file-like object without a path, pass it here.
            source must be None in that case.
    """
    def __init__(
        self,
        source: Optional[Union[str, Path, io.IOBase]] = None,
        *,
        row_tag: str = "Row",
        fileobj: Optional[io.IOBase] = None,
    ):
        self._row_tag = row_tag
        self._fileobj = None

        if source is not None and fileobj is not None:
            raise ValueError("Provide either 'source' or 'fileobj', not both.")
        if source is None and fileobj is None:
            raise ValueError("No input provided.")

        if fileobj is not None:
            # File-like object – must be readable and seekable?
            if not hasattr(fileobj, 'read'):
                raise TypeError("fileobj must have a 'read' method.")
            # For the Rust parser we need a file path; we can't stream from a fileobj directly.
            # Solution: if it's a SpooledTemporaryFile, we can use its ._file attribute
            # or fall back to writing to a temp file. For simplicity, we require a path.
            # To support file-like objects seamlessly, the FastAPI layer should use
            # NamedTemporaryFile, which always has a .name.
            # Here we try to get a path from the file object.
            if hasattr(fileobj, 'name') and isinstance(fileobj.name, (str, Path)):
                self._filepath = Path(fileobj.name)
            else:
                raise ValueError("fileobj must have a .name attribute (e.g., NamedTemporaryFile).")
        else:
            # source is a path or file-like with .name
            if isinstance(source, (str, Path)):
                self._filepath = Path(source)
            else:
                # file-like object – as above, we need a path
                if hasattr(source, 'name'):
                    self._filepath = Path(source.name)
                else:
                    raise ValueError("File-like object must have a .name attribute.")

        if not self._filepath.exists():
            raise FileNotFoundError(f"File not found: {self._filepath}")

    def schema(self) -> list[str]:
        """Return the field names of the first row without consuming the main stream."""
        # Opening a new reader each time is cheap because we stream from disk.
        first_row = next(self.__iter__(), None)
        if first_row is None:
            return []
        return list(first_row.keys())

    def __iter__(self) -> Iterator[dict]:
        if _RUST:
            return CrxmlReader(str(self._filepath), self._row_tag)
        else:
            return self._lxml_iter()

    def __or__(self, stage):
        from .pipeline import Pipeline
        return Pipeline(self) | stage

    def _lxml_iter(self) -> Iterator[dict]:
        tag = self._row_tag
        if not tag.startswith("{"):
            tag = f"{{*}}{tag}"
        context = etree.iterparse(
            str(self._filepath),
            events=("end",),
            tag=tag,
            huge_tree=True,
        )
        for _, elem in context:
            record = dict(elem.attrib)
            for child in elem:
                localname = child.tag.split("}", 1)[1] if "}" in child.tag else child.tag
                if localname == "Field":
                    key = child.get("FieldName") or child.get("Name") or "Field"
                    fv = child.find("FormattedValue")
                    val = child.find("Value")
                    text = (
                        fv.text if fv is not None and fv.text else
                        val.text if val is not None and val.text else ""
                    )
                    record[key] = text
                elif localname == "Text":
                    key = child.get("Name") or "Text"
                    tv = child.find("TextValue")
                    record[key] = tv.text if tv is not None and tv.text else ""
                else:
                    record[localname] = child.text or ""
            elem.clear()
            while elem.getprevious() is not None:
                del elem.getparent()[0]
            yield record
        del context