"""Harbor installed-agent adapter for QQ.

Layout:

- ``qq_harbor.atif``: pure-stdlib conversion from the ``qq run`` JSONL trace
  to an ATIF (Agent Trajectory Interchange Format) trajectory. Importable and
  testable without Harbor installed.
- ``qq_harbor.agent``: the Harbor ``BaseInstalledAgent`` subclass. Importing
  it requires the ``harbor`` package (see requirements.txt).

The QQ runtime remains free of ATIF/Harbor concepts; this package is the only
place the two vocabularies meet.
"""

from qq_harbor.atif import ATIF_SCHEMA_VERSION, TraceError, convert_trace, load_trace

__all__ = [
    "ATIF_SCHEMA_VERSION",
    "TraceError",
    "convert_trace",
    "load_trace",
]
