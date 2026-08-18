# Commenting

Prefer code that describes itself. Reach for a comment only when it cannot.

1. **A descriptive type first** — it makes the wrong state unrepresentable, and the compiler keeps it true.
2. **A descriptive name next** — rename until the line reads as what it does.
3. **A comment last**, for what neither can carry.

When unsure, cut. A missing comment costs a short lookup. A wrong one costs trust in every comment.

## Comment why

The code states what it does. A comment earns its place when a reader looking at this exact code would still ask why.

```rust
// GOOD: a magic value with its meaning
// 0o644 clears the group-write bit relative to the 0o664 default.

// GOOD: a reason the code cannot show
// Finder's copy engine writes com.apple.FinderInfo and reads it back to verify,
// so returning what was set lets that readback succeed.

// BAD: the code already says it
// Route the executable bits to the record.
store.set_executable(node, exec).await?;
```

A doc comment on a public item may state its contract, as long as it does not repeat the signature.

## Comment the now

The code as it stands. Not what it used to be, not the ticket, slice or PR that produced it. Version control holds history.

## Comment the thing you are on

Not code elsewhere, and not what consumes it. If the primitive documents the mechanism, a caller must not restate it — the copies will drift.

## Keep it plain

Short, active voice, literal. No metaphors, idioms or slang: name the actual function or check.

```rust
// BAD: "surface" is not a real thing in the code
//! Overlay for attributes not carried by the synced surface.

// GOOD
//! In-memory overlay for node attributes the store does not sync.
```

Say where a thing is, not where it is not. Do not state what the types already guarantee.

These rules apply to the comments already in the file. A neighbouring comment that breaks them is not a precedent to follow — delete it or fix it.
