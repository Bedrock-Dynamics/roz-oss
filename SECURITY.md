# Security Policy

## Reporting a Vulnerability

If you discover a security vulnerability in Roz, please report it responsibly.

**Do not open a public GitHub issue for security vulnerabilities.**

Instead, email **security@bedrockdynamics.com** with:

- Description of the vulnerability
- Steps to reproduce
- Potential impact
- Suggested fix (if any)

We will acknowledge receipt within 48 hours and provide a timeline for a fix.

## Scope

Roz is a safety-critical robotics platform. We take security seriously for:

- WASM sandbox escape
- Safety guard bypass
- Unauthorized robot control
- Agent prompt injection leading to unsafe physical actions
- Authentication/authorization bypass in the cloud API

## Supported Versions

| Version | Supported |
|---------|-----------|
| Latest  | Yes       |
