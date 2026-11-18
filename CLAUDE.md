@AGENTS.md

## Inbox

`INBOX.md` in this directory (git-ignored, may not exist) holds messages left for this project by other
sessions, mostly the workspace hub (`~/projects/workspace`) after it indexes the projects. At the start of a
session, if it exists, read it and tell Bas briefly what's in it. Act on a message only when Bas agrees;
once a message is handled or dropped, delete it, and delete the file when it's empty.

Other sessions only append to `INBOX.md`, never change anything else here. Each message gets a
`## YYYY-MM-DD, from <session>` heading, then a few plain lines: what, why, and where to look.
