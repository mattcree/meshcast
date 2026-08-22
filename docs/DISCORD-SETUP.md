# Setting up the Discord side

You do this once per Discord server (or once per bot instance — one bot can live in several servers). It takes about five minutes and needs no payment or verification.

## 1. Create the application

1. Open https://discord.com/developers/applications and click **New Application**.
2. Name it `Meshcast` (any name works), accept the terms, **Create**.
3. On the **General Information** page, note the **Application ID** — you'll need it for the invite link in step 3.

Optional polish: upload an icon (e.g. `packaging/icon/meshcast.png` if present) and set the description to "P2P screen streaming".

## 2. Create the bot user and token

1. In the left sidebar, open **Bot**.
2. Click **Reset Token** → **Yes, do it!** (confirm with 2FA if asked) and **copy the token**. This is shown once; keep it secret. If you lose it, reset again.
3. Under **Privileged Gateway Intents**, leave everything **off** — Meshcast doesn't need Presence, Server Members or Message Content.
4. Under **Authorization Flow**, you can turn **Public Bot** off if you don't want others inviting your instance.

The token is the only secret the bot needs. Never commit it; the deploy script stores it in a `0600` file, the Docker image reads it from `DISCORD_TOKEN`.

## 3. Invite the bot to your server

Build the invite URL (replace `APP_ID`):

```
https://discord.com/oauth2/authorize?client_id=APP_ID&scope=bot%20applications.commands&permissions=18432
```

`18432` = **Send Messages** (2048) + **Embed Links** (16384). Those are the only permissions used: the bot posts the stream card and edits it. Slash commands come from the `applications.commands` scope.

Or do it by hand: **OAuth2 → URL Generator**, tick scopes `bot` and `applications.commands`, tick permissions *Send Messages* and *Embed Links*, open the generated URL, pick your server, **Authorize**.

If you use private channels, make sure the bot's role can see and post in the channels people will `/stream` in.

## 4. Run the bot

See the README's [Run the bot](../README.md#1-run-the-bot-server-admin-once) section: one-line systemd deploy, Docker, or plain binary. Within a minute of starting, `/link` and `/stream` appear in your server (type `/` to check). Global commands can take a few minutes to appear the very first time.

The bot logs `Bot is ready as <name>` when connected to Discord and `Signal node ready` when it's on the iroh network.

## 5. Optional: let people use it from any server or DM (user install)

Discord apps can also be installed **to a user account** rather than a server, which makes `/link` and `/stream` available to that user everywhere. If you want that:

1. **Installation** page → tick **User Install** under *Installation Contexts*.
2. Under *Install Link* choose "Discord Provided Link" and share it.
3. Note: when used as a user-installed app, the bot can only post the public stream card where the invoking user could post themselves.

Meshcast's commands are registered without explicit context restrictions, so they work in both contexts.

## 6. Moving or reinstalling the bot

The bot's identity (its iroh key) and every user's link topic live in its state file:

- systemd system service: `/var/lib/meshcast-bot/state.json`
- systemd user service: `~/.config/meshcast-bot/state.json`
- Docker: the `/data` volume

Copy that file to the new host and nobody needs to `/link` again. Lose it and every user must `/link` once more (the old links are harmless; the app's *Unlink* button removes them).

## Troubleshooting

| Symptom | Check |
|---|---|
| Commands don't appear | Was `applications.commands` in the invite? Wait a few minutes; try another channel; restart the Discord client. |
| `/link` says "Something went wrong" | Bot logs (`journalctl -u meshcast-bot -f`). Usually the iroh endpoint isn't online yet — wait for `Signal node ready`. |
| Bot can't post the stream card | It needs *Send Messages* + *Embed Links* in that channel (check channel-level overrides). |
| Token rejected on start | Reset the token in the portal and re-run `deploy-bot.sh` with the new one. |
