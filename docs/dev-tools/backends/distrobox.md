---
description: "Run commands from an OCI image through a distrobox container, exported as mise tools."
---

# distrobox backend <Badge type="warning" text="experimental" />

The `distrobox` backend makes commands from an OCI container image available as
mise tools. It creates a [distrobox](https://distrobox.it) container from the
image and exports the commands you list with `distrobox-export`. Each exported
command is a small wrapper that runs the real command inside the container, with
your home directory and working directory shared.

Use it for tools that are packaged for another Linux distribution but not for
yours, or that need a full userland that a single binary download cannot provide.

::: warning Experimental
This backend may change. Installing a tool with it needs `experimental = true`,
and `mise edit` offers it only when that setting is on.
:::

## Requirements

- Linux
- `distrobox` on `PATH`, or its path in the [`distrobox`](#distrobox) tool option
- podman, docker or lilipod, which distrobox uses to run the container

## Usage

The tool name is the image without a tag, a `:`, and a comma-separated list of
commands to export. The version is the image tag:

```sh
mise use distrobox:registry.fedoraproject.org/fedora-toolbox:htop,jq@latest
```

mise creates a container from `registry.fedoraproject.org/fedora-toolbox:latest` and writes `htop`
and `jq` wrappers to `bin/` in the install directory, so they are on `PATH` like
any other mise tool. The first run of an exported command can be slow, because
distrobox starts the container.

A command is either a name, which mise looks up on the container's `PATH`, or an
absolute path inside the container:

```sh
mise use distrobox:ghcr.io/acme/tools:/opt/acme/bin/acme@1.4
```

An image name without a registry uses Docker Hub, as with `docker pull`.

### Versions

`mise ls-remote distrobox:registry.fedoraproject.org/fedora-toolbox:htop` lists the image's tags
from its registry. Credentials come from the same places docker and podman read
them (`~/.docker/config.json`, `$REGISTRY_AUTH_FILE`, and so on).

`latest` installs the image's `latest` tag when it has one. Registries list tags
alphabetically, so for images without a `latest` tag, prefer an explicit tag.

A tag such as `latest` can move. `mise install --force` recreates the container
and pulls the image again.

## Tool options

### `packages`

Space-separated packages that distrobox installs with the image's package
manager while creating the container, for commands the image does not ship:

```toml
[settings]
experimental = true

[tools."distrobox:docker.io/library/debian:rg"]
version = "bookworm"
packages = "ripgrep"
```

### `distrobox`

The `distrobox` executable to run, if it is not on `PATH`.

## Uninstalling

`mise uninstall` removes the exported wrappers and the container mise created
for that tool version. The image stays in your container engine's storage; remove
it with `podman image rm` or `docker image rm`.
