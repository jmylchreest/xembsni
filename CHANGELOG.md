# Changelog

## Unreleased

### Fixed

- Deliver tray button events to the widget listening for each event, with its
  own coordinates, including when decorative child windows cover the icon.
- Place tray menus using the current desktop's work area instead of always
  using the first desktop's panel exclusions.
