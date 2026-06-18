# Security Policy

## Reporting a vulnerability

Do not open a public issue or pull request for a security problem.

Report it privately via a GitHub security advisory at https://github.com/bllshttng/footnote/security/advisories, or by emailing the repository owner. Please include enough detail to reproduce (affected version or commit, steps, and impact). You will get an acknowledgement, and a fix or mitigation will be coordinated before any public disclosure.

## Trust boundary (read this before running plans)

footnote is not a sandbox. The autonomous loop executes the plan you point it at, with your credentials, on your machine. That is the intended design for a single operator running their own work; it is also the main thing to understand about its security posture.

- Plans you wrote, or reviewed, are the expected input.
- Plans from untrusted sources (random contributors, gists, copied bug reports) should be read carefully before you run them. The loop will execute them with your credentials.
- There is no multi-tenant isolation. Container/sandbox abstractions are out of scope.

For the full posture, including what the loop will and will not do given a plan and what it cannot prevent, see [docs/security-posture.md](docs/security-posture.md).
