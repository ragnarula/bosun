# ADR: A persona's avatar is a seeded robot drawn by the client, or a picture the control plane redraws

**Date:** 2026-10-08
**Author:** Pete

## Context

The pane and the terminal client show each agent in a session tree as a crew member. A crew member needs a face that says which persona it is and tells two agents of one persona apart, without looking human: a human face raises trust beyond what an agent has earned. The control plane runs offline on machines the user owns, and the pane has no build step and loads nothing from a CDN.

## Decision Drivers

- Works offline, with nothing fetched from outside the control plane.
- The same face for the same agent in every client and on every load.
- A user can replace a persona's face with their own picture.
- An upload must not become a way to serve script or leak metadata.

## Options Considered

**1. A robot drawn in the browser from a seed, and an optional uploaded picture redrawn by the control plane. (chosen)**

The pane's own module draws a robot as inline SVG from a seed string, built from shapes, so the same seed gives the same robot. The persona's seed is stored and can be shuffled. An uploaded picture is decoded and redrawn as a 256-pixel PNG before it is stored.

**2. DiceBear's styles rendered by the control plane in Rust. (rejected)**

It adds a dependency and a render route for something the pane can draw in a few dozen lines, and its friendliest styles carry artist licence terms. The robots here are Bosun's own.

**3. AI-drawn avatars. (rejected for now)**

They need an image model, a key and the network, and cost money per image. Nothing in this decision prevents adding them later as one more way to set a picture.

**4. Store uploads as sent. (rejected)**

An SVG can carry script, and a JPEG carries EXIF data such as location. Serving the upload's bytes would serve both.

## Decision

- `persona_avatars(persona, seed, picture, updated_at_secs)` holds a row for a persona whose seed was shuffled or whose picture was uploaded. A persona with no row uses its name as its seed.
- `GET /personas` adds `avatar_seed` and `picture_at_secs` to each persona.
- `POST /personas/{name}/avatar/shuffle` stores a new random seed and answers it.
- `PUT /personas/{name}/avatar` takes a PNG, JPEG or WebP of at most 5 MiB, decodes it with decode limits of 8192 pixels a side, crops it to a centred square, scales it to 256 pixels, and stores the PNG it encoded. Anything else is refused with 400.
- `GET /personas/{name}/avatar` serves that PNG with a long cache lifetime; clients put `picture_at_secs` in the URL so a new picture is fetched. `DELETE` removes the picture and keeps the seed.
- A child session's robot is drawn from its session id, so two builders differ; a persona's picture, when set, is used for all its sessions.
- Only configured personas take avatar writes.

## Consequences

- Faces work offline and cost nothing to draw.
- The robot drawing lives in the pane's module; the terminal client shows a coloured tag instead, so the two clients do not share a drawing.
- A picture replaces the face of every session of that persona, so two builders with a picture look alike and are told apart by name and number.
- The control plane carries the `image` crate's PNG, JPEG and WebP codecs.

## Revisit When

- AI-drawn avatars are wanted.
- A user needs a picture per session rather than per persona.
