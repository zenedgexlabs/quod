"""Quod Python probe: records which code, files and packages each test observes."""

__version__ = "0.4.5"
PROBE_ID = f"python-probe/{__version__}"
CAPTURE_VERSION = 1
# Capabilities declared in the capture header. ``py.introspection``: signature
# and annotation introspection is recorded as a use of the function.
# ``py.environment``: the header carries environment and invocation
# fingerprints, and environment variable reads are recorded (ADR 012).
CAPABILITIES = ("py.environment", "py.introspection")
