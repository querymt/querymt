# Themes converted from Zed

These files are not copied from gpui-kit. They are converted from the
themes built into the Zed editor into the gpui-kit theme format by
`convert_zed.py` in this folder (openspec change `desktop-themes`, design D11),
and the output is committed. The script is not part of the build.

- Source: https://github.com/zed-industries/zed/tree/150c2aaeca8778c2759bba92b19247b0901d536f/assets/themes
- Commit: `150c2aaeca8778c2759bba92b19247b0901d536f`
- Licences: recorded in Zed's `assets/themes/LICENSES` at that commit. Ayu,
  Gruvbox and One are each listed there under the MIT licence.

Zed's colour names are mapped to the kit's by the table in design D11; the
colours themselves are Zed's values, not adjusted.

| File | Themes | Upstream project | Upstream licence |
|---|---|---|---|
| `ayu-mirage.json` | Ayu Mirage | https://github.com/dempfi/ayu | MIT, copyright (c) 2016 Ike Ku (per Zed's `LICENSES`) |
| `gruvbox-variants.json` | Gruvbox Dark Soft, Gruvbox Light Hard, Gruvbox Light Soft | https://github.com/morhetz/gruvbox | MIT per Zed's `LICENSES`; the copyright line there is the unfilled template (`<YEAR> <COPYRIGHT HOLDER>`), so the holder is not confirmed |
| `one.json` | One Dark, One Light | https://github.com/atom/atom (`one-dark-ui`, `one-light-ui`) | MIT, copyright (c) 2014 GitHub Inc. (per Zed's `LICENSES`) |

Zed's Gruvbox Dark and Gruvbox Dark Hard are not converted: the kit's Gruvbox
Dark (`../kit/gruvbox.json`) already uses gruvbox's hard background `#1d2021`.
