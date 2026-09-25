---
name: bosun-deployment
description: Use after a Bosun release is published, to put it on the machines that run it — starts a session on the egadd node's Bosun deployment directory and follows its instructions, and pings @ragnarula on the issue when starting that session is not yet possible.
version: 0.1.0
---

# Bosun Deployment

A release is not deployed until the machines that run Bosun are on it. One release, one deployment.

The procedure belongs to the lab, not here: it lives in the `egadd` node's directory `code/active/egadd-lab/infra/bosun`, which holds that environment's own instructions and credentials. This skill is only about how the deployment gets run and what to do when it cannot be.

## 1. Confirm the release is real

The tag's workflow finished `completed success` and the release carries its assets:

```bash
curl -s "https://api.github.com/repos/ragnarula/bosun/actions/runs?per_page=3"
curl -s "https://api.github.com/repos/ragnarula/bosun/releases/tags/vX.Y.Z"
```

Done when you can name the version the machines must move to.

## 2. Run the deployment on egadd

The deployment is a session on the `egadd` node in `code/active/egadd-lab/infra/bosun`, so it runs where the environment is and with its credentials.

Start one there with the instruction to deploy `vX.Y.Z`, then watch it the way you watch any child session: read what it reports, answer its questions, and do not do its work for it. How you start it depends on what the tooling can do:

- `bosun clone` and `bosun dev` take `--node egadd`, and the pane's new-session sheet lists nodes. Use those when you are driving the CLI or the pane yourself.
- If the cross-node spawn tool exists (see issue #12, "Let an agent start a session or subagent on another node"), use it from your own session.

Done when the session on egadd exists and reports the version it deployed, or reports the step that failed.

## 3. When you cannot start that session

An agent cannot yet start a session on another node — `spawn` runs the child on its own node in its own working copy. Until issue #12 lands, the deployment waits for the operator:

- Comment on the issue the release closed, pinging `@ragnarula`, naming the version to deploy and the directory the deployment runs in.
- Say plainly on the issue that the release is built and undeployed, so nobody reads the closed issue as finished.

Done when that comment is posted. The issue stays closed; the comment is the hand-off.

## 4. Check it landed

Once something reports a deployment, verify the running control plane answers with the new version rather than trusting the report: the pane and the CLI both carry `X-Bosun-Version`, and `bosun nodes` lists each node's version and whether an update is outstanding.

Done when the control plane and every node that runs Bosun report `X.Y.Z`, or a node is named as still behind with the reason.
