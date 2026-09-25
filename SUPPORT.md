# Support

LensQuery is a side project maintained in spare time. Support is **best
effort, no SLA, PRs welcome.** That is the whole policy, written down so it
is not a surprise to anyone.

## Before opening anything

Run `lq doctor` and read its output. It prints the resolved Tesseract library
and every path it tried, the visible language packs, the database path and
schema version, and the active defaults — which is the answer to a large share
of the questions people have.

Then check [docs/troubleshooting.md](docs/troubleshooting.md). It is organised
symptom first.

## Where to go

| You have | Go to |
| --- | --- |
| A bug, with a reproduction | GitHub Issues — include the full `lq doctor` output |
| A crash or wrong result | GitHub Issues — include the command, the OS, and `lq doctor` |
| A question about how something works | GitHub Discussions |
| A feature idea | GitHub Discussions first, so the design conversation happens before the code |
| A security issue | Do **not** open an issue. See [SECURITY.md](SECURITY.md) |

## What is likely to get a fast answer

- Anything with a `lq doctor` dump attached.
- Anything reproducible on the committed fixtures in `tests/fixtures/`.
- Platform bugs on macOS and Linux, which get less real-hardware use than
  Windows and therefore need reporters more.

## What is likely to be declined

- Requests to add a server, a daemon, a web UI, or a container. "One binary,
  no server" is the point of the tool, not an unfinished state.
- Requests to make `--fuzzy` do typo correction without a measurement. The
  current behaviour is a deliberate, documented trade.
- Bug reports without the command that produced them.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). The two contributions actively worth
soliciting are new OCR backends and new entries in the language catalogue.
