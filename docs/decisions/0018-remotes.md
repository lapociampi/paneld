---
title: "ADR 0018: Remotes"
description: "Describe a machine or service once, with how to reach it and what it exposes, and use it as device1.temp."
date: 2026-11-04
tags: [paneld, adr, config]
series: paneld
order: 25
---

- Status: Accepted. Builds on [ADR 0005](0005-commands-only.md) and amends [ADR 0013](0013-command-runner.md) (stdin).
- Date: 2026-11-04

## Context

A panel mostly watches and drives other machines and services. With only `source` and `action`, every item repeats how to reach its target (`ssh -o BatchMode=yes admin@10.0.0.5 ...`, `curl -sf -H ... https://nas/api/...`), five identical boxes need five copies, and the safe way to pass a UI value through ssh (quote it for the remote shell) or into a URL (encode it) is left to each config author.

## Decision

### A kind and its instances

| Item | Says |
| --- | --- |
| `remote_kind <name> { }` | How to connect (`via` and its keys), the `param`s that differ per instance, and what it exposes: `value <name> { }` (a source) and `action <name> { }` |
| `remote <name>: <kind> { }` | One instance: its params (`host = 10.0.0.5`) |

- Kinds usually live in files of their own (`include = remotes/*.conf`), so a kind can be shared between configs.
- Kind params are literals (or `$vars`), read at load; inside the kind they are plain names (`{{ host }}`), and they win over any page name (a `repeat` binding called `host`). Connection keys (`host`, `url`, the kind's `headers`, ...) read the params and `secrets` only, so nothing on screen can change where a request goes. The kind's `timeout` is its members' default. A `secret` param names a `secret` item, so two boxes of one kind can have their own tokens; it is usable only where `secrets` is.
- In expressions, `device1.temp` is the value; `device1.ok` / `device1.error` the remote as a whole; `device1.status.temp` the full source map (`ok`, `error`, `at`, `history`, ...). `device1.reboot` is an action anywhere an action fits (`on_tap = device1.reboot`, `refresh = device1.temp`, `alert ... from = device1.log`). Dotted members are not in `sources`.
- `remote[device1]:host = 10.0.0.9` overrides one param, like any named item.
- A kind can declare `alert <name> { }`: each instance gets its own, `<instance>.<name>`, whose expressions read the kind's params and `self` (the instance's root, `self.temp`). Five boxes of one kind need no five hand-written alerts.
- Names are `[a-z][a-z0-9_]*`, not a CEL keyword. A kind param may not be named like an expression root (`secrets`, `vars`, ...), which it would hide in its members. An instance name may not be an expression root, nor a `repeat` binding or a param name anywhere (it would shadow the instance).

### Connections wrap system tools

The program still only runs commands ([ADR 0005](0005-commands-only.md)). `via` picks how paneld builds the command around a member's `run` (or request):

| `via` | Command | Notes |
| --- | --- | --- |
| `local` | the `run` as is | For kinds describing the panel itself |
| `ssh` | `ssh` with fixed options, `-- <host> <quoted run>` | See below |
| `http` | `curl -q -K -`, the request on stdin | See below |
| `socket` | `socat -T <timeout> - UNIX-CONNECT:<path>` or `TCP:<host>:<port>` | `send` goes to stdin; the reply is the output |

Every run keeps the ADR 0013 rules: clean environment, timeout, output limits, `app:max_running`, masking of secrets. Amending ADR 0013: an `http` or `socket` run gets its request on stdin; every other run still reads `/dev/null`.

#### ssh

- Options: `-T -F none -o BatchMode=yes -o IdentitiesOnly=yes -i <key> -o StrictHostKeyChecking=yes|accept-new -o UserKnownHostsFile=<known_hosts> -o GlobalKnownHostsFile=/dev/null -o UpdateHostKeys=no -o ConnectTimeout=<min(timeout, 10s)> -o ServerAliveInterval=15 -o ServerAliveCountMax=3 -o LogLevel=ERROR -p <port> -l <user> -- <host>`.
- `key` is required: under the service user there is no home folder, so there is no default key. ssh refuses a key others can read.
- `known_hosts` defaults to the state folder (`/var/lib/paneld/known_hosts`), which the service can write; the config folder is read-only to it.
- Each argv item of `run` is single-quoted for the remote shell, so a UI value stays one argument there too. The remote login shell must be POSIX-like (sh, bash, dash, zsh); Windows and network-device CLIs are not supported.
- `secrets` are refused in an ssh `run` (they would be on ssh's argv and in the remote process list) and `env { }` is refused (OpenSSH passes it only where the server allows it).
- Exit code 255 is ssh's own failure (cannot connect, host key mismatch): `error` says so. A remote command that exits 255 looks the same.
- Killing ssh (timeout, reload, a stream restart) does not always stop the remote command; `ServerAlive` ends a stream's remote side once the link is gone. A timed-out remote action is reported as "timed out; it may still have run".
- No ControlMaster: each run is a full connection. Poll intervals should allow for that on a Pi.

#### http

- The request is a curl config file on stdin: `url`, `request`, `header`, `user` (basic auth) and `data-raw` lines, each value in double quotes with `\`, `"`, CR, LF and tab escaped, plus `fail-with-body`, `path-as-is`, `proto = "=http,https"`, `max-time`, `silent`, `show-error` (and `cacert` / `insecure`). `-q` keeps `~/.curlrc` out. Tokens are never in argv.
- No redirects are followed.
- A `{{ }}` in a request path (`get = /vms/{{ vm }}/status`) is percent-encoded: everything but RFC 3986 unreserved characters, and a value that is just `.` or `..` whole, so `/`, `?`, `#`, `&`, `%` and dot steps cannot change the request. The rest of the path is used as written; `globoff` keeps `{}` and `[]` literal.
- CR, LF or NUL in a URL or header value refuse the run. Header names are config keys, so `_` in one is sent as `-` (`X_Api_Key`).
- `secrets` are allowed in `url`, `headers` and `body`, not in a request path.
- An HTTP status of 400 or more fails the run (curl exit 22) with the body as output; `result.code` is curl's exit code, and `result.status` (and a value's `status`) the HTTP status, written by curl after the response and taken back out of stderr.

#### socket

- `path` must not contain `,`, `!` or whitespace; `host` is a name or an IP address (IPv6 in brackets). Other characters would be read as socat address options.
- The reply ends when the peer closes, or a second before `timeout` (`-T`, `-t`), so a peer that keeps the connection open still yields what it sent; the run is ok when socat exits 0.

### Status

- `device1.ok`: at least one member value has a result and none has failed since; `false` before the first result (as section 4 does for sources). A run with no result (timeout, unparseable output, a stream that stopped or cannot start) sets the value's `ok` and `error` and keeps the rest of its last result; a stream that runs again is `ok` again.
- A member of an instance that a reload removed refuses to run; it never falls back to running on the panel.
- `device1.error`: the error of the first failing member value in declaration order, else `""`.

### Lowering

After the check, each instance becomes ordinary sources and actions named `<instance>.<member>`, carrying their remote. Pollers, streams, history, alerts, `refresh`, the gates (PIN, confirm, params) and the action log need no remote-specific code; only building the command does.

## Consequences

- `ssh`, `curl` and `socat` must be installed for the kinds that use them; `paneld check` does not look for them.
- A remote member's `run` is argv, not shell text, exactly as a local one: pipes need `sh -c '...'` (run on the remote), and the inline-script lint applies. Free-text params can still start with `-`; write `--` before them (`systemctl restart -- {{ unit }}`) or give them `options`.
- No native protocol code: no TLS or ssh library in the binary. A future `via` is a new command builder, not a client.
- Member names `ok`, `error` and `status` are reserved.
- Open: a `via` override per instance, ssh connection reuse.
