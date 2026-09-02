# wmde-screenshot

Screenshot tool for WMDE. Reference: flameshot.

Written here rather than forked: COSMIC ships no screenshot client. The capture half moves
in from `wmde-portal`, which keeps the `org.freedesktop.impl.portal.Screenshot` interface
as a thin forward so third-party applications reach this program too - one screenshot
interface in the system, not two.

The binary is both halves of the design:

    wmde-screenshot           # become the service on fun.wmde.Screenshot
    wmde-screenshot gui       # ask the running service for the interactive overlay
    wmde-screenshot full      # every output, no interface
    wmde-screenshot screen    # the output under the pointer
    wmde-screenshot window    # the focused window

The service is started by the session bus on the first request, through
`/usr/share/dbus-1/services/fun.wmde.Screenshot.service`, and stays resident afterwards so
that the path from key press to overlay holds no toolkit start-up.

Handbook: `.doc/screenshot/` in the WMDE tree.
