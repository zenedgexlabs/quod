# quod-js

The Vitest and Jest probe for [Quod](https://github.com/zenedgexlabs/quod):
sound, certificate-based test selection. It records which functions each
test actually runs (V8 precise coverage), and runs only the tests a change
obligates.

Most users don't install it directly: the
[GitHub Action](https://github.com/marketplace/actions/quod-verdict) does.
To use it with the `quod` CLI (Node 22+, Jest needs 22.12+):

```sh
npm install --save-dev quod-js

npx quod-js record --out capture.jsonl -- <runner args>
quod ingest --capture capture.jsonl --out map.qmap
quod select --map map.qmap --advisory --skip-out skip.txt
npx quod-js run --skip skip.txt --results results.jsonl -- <runner args>
```

The runner is detected (Vitest if configured, else Jest), or set it with
`--runner vitest|jest`. Your config is wrapped, never modified. See
[supported versions and known limits](https://github.com/zenedgexlabs/quod/blob/main/docs/limits.md).

Licensed under the Apache License 2.0.
