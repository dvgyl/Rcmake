# rcmake documentation

| Question | Document |
| --- | --- |
| How do I install and use `rcmake`? | [README.md](../README.md) |
| What are the common questions about compatibility, caching, native tools, and limits? | [FAQ.md](FAQ.md) |
| Where are the concise and full machine-readable references for LLMs? | [site/llms.txt](../site/llms.txt), [llms.txt](https://dvgyl.github.io/rcmake/llms.txt), and [llms-full.txt](https://dvgyl.github.io/rcmake/llms-full.txt) |
| How was `rcmake` built and tested, and how can I reproduce each claim? | [BUILD_WRITEUP.md](BUILD_WRITEUP.md) |
| How does a configure run flow through the 35 crates? | [ARCHITECTURE.md](ARCHITECTURE.md) |
| Why was a design decision made? | [ADR.md](ADR.md) (31 architecture decision records) |
| Which environment variables change `rcmake` behavior? | [ENV_VARS.md](ENV_VARS.md) |
| Which third-party code and crates does `rcmake` use, and under which licences? | [PRIOR_ART.md](PRIOR_ART.md) |
| How do I make my first change and open a pull request? | [CONTRIBUTING.md](../CONTRIBUTING.md) |
| How do I report a security problem? | [SECURITY.md](../SECURITY.md) |
| How do I run configure and `explain` from my own Rust program? | [crates/rcmake-api/README.md](../crates/rcmake-api/README.md) |
| How do I connect my editor to the language server? | [crates/rcmake-lsp/README.md](../crates/rcmake-lsp/README.md) |
| How do I enable shell completions or read the man page? | [completions/README.md](../completions/README.md) and [man/rcmake.1](man/rcmake.1) |

The [compatibility report](https://dvgyl.github.io/rcmake/dashboard.html) shows the latest verification results. The [browser playground](https://dvgyl.github.io/rcmake/playground.html) runs `rcmake` with no install.
