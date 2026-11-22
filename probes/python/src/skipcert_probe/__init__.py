"""Skipcert Python probe: records which code, files and packages each test observes."""

__version__ = "0.2.0"
PROBE_ID = f"python-probe/{__version__}"
CAPTURE_VERSION = 1
# Capabilities declared in the capture header. ``py.introspection``: signature
# and annotation introspection is recorded as a use of the function.
CAPABILITIES = ("py.introspection",)
