# Nova Privacy Policy
Last updated: 27 October 2026

Nova is developed by Nakul Srivastava and published by JagatBandhu. This policy describes Nova 0.6.0 and its optional integrations.

## Local processing
Nova processes desktop features on your device. Depending on the features you use, this can include media titles, artists and artwork, fetched lyrics, calendar event details, prayer-reminder coordinates, reminders and status items, selected images, Shelf file paths, clipboard text and image dimensions, Windows notification content, and messages sent to its local notification endpoint. Glass rendering reads desktop pixels behind the notch into memory. No Nova account is required.

## Local configuration
Preferences and content you enter, including reminder text, priorities, image paths, Shelf paths and folder settings, and calendar feed URLs, are saved in a local configuration file, normally %APPDATA%\nova\config.json. This file is not encrypted. Treat private calendar links as secrets. Nova can import an older configuration from %APPDATA%\venu\config.json or %APPDATA%\movingtext\config.json. A local startup log at %LOCALAPPDATA%\Nova\startup.log records limited lifecycle and diagnostic information.

## Shelf
Shelf stores paths rather than copies of your files. It reads configured folders to show recent files and can generate local thumbnails in memory. Selecting a pinned item can open the file or folder, launch an application, or run a script using Windows. Only pin and open items you trust.

## Clipboard history
Clipboard collection is off by default. If enabled, Nova reads Unicode text from the Windows clipboard and keeps its history in memory, with text limited to 4,000 characters per entry. For screenshots and copied images it keeps only the latest dimensions; pixel data remains owned by the Windows clipboard. It honors supported private-clipboard markers and the Windows flag that excludes content from clipboard history, but cannot guarantee that every sensitive item is marked by its source. Avoid copying sensitive content while history is enabled. Disabling clipboard history clears its in-memory entries. Clipboard history is not saved as a permanent archive.

## Windows notifications
In the packaged Store version, Windows notification collection is optional and off by default. After you connect it and grant Windows permission, Nova can read current Windows toast notifications, including the source app, title, body, and time, and display them in the notch. Retained records are in memory only and are not a permanent or restart-persistent archive. Removing a notification from Windows does not necessarily remove the retained Nova record immediately. Disconnecting stops collection but does not itself clear already retained notifications; use Nova dismissal or clear controls, or exit Nova, to remove them from memory. You can revoke access through Windows settings.

## Other in-memory data
Media details, fetched lyrics, parsed calendar events, image and rendering caches, and Nova notifications are held in memory for the application session. Original images, files, and calendars remain under your control.

## Optional network connections
If you configure an online iCal feed, Nova downloads it directly from the URL you provide. When lyrics are enabled, Nova sends the active track title, artist, optional album and duration to LRCLIB only when the track changes. Providers receive the request and ordinary connection information such as your IP address. A local ICS file can be used instead. Automatic prayer location uses Windows Location Services only after you press the detection button; Nova stores the resulting coordinates locally and does not continuously track location.

## Developer integrations
Optional scripts send event messages to a local HTTP endpoint, normally 127.0.0.1:18923. Some notification hooks can include an excerpt from tool output. Review the scripts and avoid sending sensitive content. The endpoint is bound to this computer and is not authenticated; other local software can submit events.

## Your choices and deletion
Disable optional collection and integrations in Settings, remove Shelf entries and calendar feeds, and clear Nova notifications using the in-app controls. To reset stored preferences, exit Nova and delete %APPDATA%\nova. If present, remove the legacy %APPDATA%\movingtext\config.json to prevent it being imported again. Delete %LOCALAPPDATA%\Nova\startup.log to remove the diagnostic log. Paths can differ because of Windows packaged-app storage behavior. Remove separately configured agent hooks to stop their submissions. Do not rely on uninstalling alone to remove all settings or other local data. These steps do not delete your original pinned files, images, scripts, or calendar files.

## Questions
Contact the developer through https://github.com/hfzrfa/venu-notch/issues. Do not post private calendar links, clipboard contents, notification text, or other sensitive data in a public issue.

This policy will be updated if Nova introduces new data-handling features.
