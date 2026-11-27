# quod-probe

The pytest plugin for [Quod](https://github.com/zenedgexlabs/quod): sound,
certificate-based test selection. It records what every test actually
depends on (functions, packages, environment variables, module state), and
runs only the tests a change obligates.

Most users don't install it directly: the
[GitHub Action](https://github.com/zenedgexlabs/quod/tree/main/integrations/github-action)
installs it. To use it with the `quod` CLI:

```sh
pip install quod-probe            # Python 3.12+, pytest 8+

quod record --out map.qmap -- python -m pytest
quod select --map map.qmap --advisory --skip-out skip.txt
python -m pytest --quod-skip=skip.txt --quod-results=results.jsonl
```

The plugin does nothing unless a `--quod-*` option is given.
See the [Quod README](https://github.com/zenedgexlabs/quod#readme) for the
full workflow, and [supported versions and known
limits](https://github.com/zenedgexlabs/quod/blob/main/docs/limits.md).

Licensed under the Apache License 2.0.
