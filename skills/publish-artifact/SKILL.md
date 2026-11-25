---
name: publish-artifact
description: Publish an image, video, HTML file, or localhost preview for inspection in a Power Overwhelming workspace.
---

When producing an artifact for the user, run `power artifact publish <path-or-url>` from the Power Overwhelming session. Pass the actual local file path or the running loopback URL. For HTML with local assets, publish its entry file and keep the assets alongside it. Include the returned `pwr://artifact/...` URI verbatim on its own line so the user can click it in the terminal. If publication fails, report the error and give the original path or URL.
