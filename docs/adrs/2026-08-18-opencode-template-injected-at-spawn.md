# ADR: One opencode config template injected at spawn

**Date:** 2026-08-18
**Author:** Raghav

## Context

The architecture routes every outbound agent connection through the supervisor and has workers store no secrets. The MVP explicitly trades security away: one person, trusted machines. `opencode serve` needs a provider key and model configuration to be useful.

## Decision Drivers

- A spawned session must work with the person's provider and key without per-machine setup.
- Changing provider must be one edit, not one edit per node.
- No secret store, no key forwarding, no proxy of model traffic in this phase.

## Options Considered

- **Keys configured on each node directly.** Rejected: the provider choice and key then live in every node's config, and changing provider means editing every node.
- **Control plane proxies the model API and holds all keys.** The architecture's full model. Rejected: it adds a traffic-forwarding layer and a key store for a benefit the single user does not need yet.
- **One template injected at spawn (chosen).** The control plane reads a single `opencode.json` template and sends its contents with each spawn request. The node writes it into the session's clone directory before starting `opencode serve`.

## Decision

The control plane holds one `opencode.json` template, including the provider key. At spawn it sends the template contents to the node, and the node writes it to `<clone>/opencode.json` before starting the server. The key travels control plane to node over the trusted network at spawn time.

## Consequences

- Changing provider or model is one edit to the template; the next spawn anywhere uses it.
- The key is exposed on the wire and on the node's disk. That is accepted for this phase.
- The template content becomes part of the spawn request contract, so a future key store swaps in behind the same field.

## Revisit When

Security matters — when machines leave the trusted network, or more than one person uses Bosun. The template field then becomes a key reference instead of the key itself.
