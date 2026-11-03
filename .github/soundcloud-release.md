Sonora with SoundCloud support, for Linux and Windows (64-bit). Each download is the whole of Sonora with these changes built in, so it installs on a computer that never had Sonora, and replaces a normal Sonora where there is one.

What it adds to Sonora:

- **SoundCloud**, as a guest or signed in, with your likes, playlists and followed artists.
- **Two services at once**: sign in to both, then open Settings → Accounts and press **Add to …** on the second one. Both libraries and both searches show together.
- **Sonora playlists** that mix songs from every service. Only the list of songs is kept on your device; every song still streams.
- **Stats**: minutes listened, streams, and your top songs, artists and albums for today, the past weeks and months, this year or all time, with a listening clock.

It updates itself from this repository, never from upstream Sonora.

### Windows

In PowerShell:

```powershell
irm https://raw.githubusercontent.com/lvyrize/sonora-soundcloud-support/feat/soundcloud/scripts/install-soundcloud.ps1 | iex
```

Or download **Sonora-SoundCloud-Setup.exe** below and run it. To go back to the normal Sonora:

```powershell
& ([scriptblock]::Create((irm https://raw.githubusercontent.com/lvyrize/sonora-soundcloud-support/feat/soundcloud/scripts/install-soundcloud.ps1))) -Uninstall
```

### Linux

```sh
curl -fsSL https://raw.githubusercontent.com/lvyrize/sonora-soundcloud-support/feat/soundcloud/scripts/install-soundcloud.sh | sh
```

To go back to the normal Sonora:

```sh
curl -fsSL https://raw.githubusercontent.com/lvyrize/sonora-soundcloud-support/feat/soundcloud/scripts/install-soundcloud.sh | sh -s -- --uninstall
```

Run the install command again at any time to update.
