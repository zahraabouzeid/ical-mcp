# iCal MCP Server

A Model Context Protocol server that provides read-only access to calendars
through their iCal links. This server enables LLMs to look up events, including
repeating events, for a range of days.

## Available Tools

- `list_events` - Lists the events of the connected calendar for a range of days.

## Usage

Add the server as a custom connector in an MCP client:

`https://ical.mcp.zahraabouzeid.me/mcp`

The sign-in page asks for the iCal or webcal link of the calendar.

## Security

This server stores the calendar link encrypted on the server. A connection unused for 90
days is deleted. Only connect a calendar if you trust the operator of this server.

## License

This project is licensed under the [AGPL-3.0](LICENSE).
