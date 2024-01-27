# Guide: Publishing

Publishing means associating data with your id and publicizing it so that anyone else who knows your id can access it. This data can be anything - IP addresses, keys, names, contact info, status updates, etc.

In order to publish, you'll need:

- An [identity](./guide_identities.md)
- A publisher - either [set your own up](./reference_spagh_node.md) or get your identity authorized on a publisher run by someone else

## Manual publishing

To manually publish, you'll need the [`spagh`](./reference_spagh.md) - follow that link for setup instructions.

Suppose you have a JSON file

```json
{
  "data": {
    "serial_number": {
      "ttl": 60,
      "data": {
        "long": "1234123412341234-1234",
        "short": "1234"
      }
    }
  },
  "missing_ttl": 5
}
```

and you want to publish it under your identity `me.spagh` with ID `yryyyyyyyyei1n3eqbew6ysyy6ocdzseit6j5a6kmwb7s8puxmpcwmingf67r`.

Set the environment variable `SPAGH` to the URL of your publisher and run

```
$ spagh set local me.spagh ./data.json
```

Anyone can now look it up by doing

```
$ spagh get yryyyyyyyyei1n3eqbew6ysyy6ocdzseit6j5a6kmwb7s8puxmpcwmingf67r serial_number
{
    "long": "1234123412341234-1234",
    "short": "1234"
}
```

### Notes about published data

All key/value pairs you want to publish for this identity must be in the JSON - this is in order to support negative caching via `missing_ttl`.

The JSON matches [this schema](TODO).

- The first `data` contains the key value pairs of data to be published.

- The second `data` is an arbitrary JSON value associated with the key that will be returned from queries.

- `missing_ttl` (minutes) is how long a "missing" response will be cached (a resolver can say the data is missing for this long without performing another lookup).

- `ttl` (minutes) is how long a successfully resolved value can be cached by a resolver.

### Publishing DNS bridge records

The DNS bridge allows accessing keys and values with a specific format via DNS, so you can (for example) type an identity into your browser address bar and access an IP published for that identity in Spaghettinuum.

Spaghettinuum key/value pairs that match the format expected by the DNS bridge will be referred to as DNS equivalent records.

The easy way to publish DNS equivalent records is using the command line like:

```
$ spagh set-dns local me.spagh --a 256.256.256.256 --aaaa 2001:db8::8a2e:370:7334
```

You can also do it using the normal `set` command. In that case

- Use key `a6ff2372-e325-443f-a15f-dcefb6aee864` for CNAME records, with data in [this format](TODO)
- Use key `dff50392-a569-4de4-9e66-e086af040f30` for A records, with data in [this format](TODO)
- Use key `a793cc93-cc06-4369-ba47-5a9e8d2a23dd` for AAAA records, with data in [this format](TODO)
- Use key `630e1d90-845a-470f-95f3-14253a6c269c` for TXT records, with data in [this format](TODO)
- Use key `f665bd5f-6da7-4fa7-8ef9-51dd9a53ff60` for MX records, with data in [this format](TODO)

## Setting up a static file server

The `spagh-auto` is the simplest way to set up a static file server, and will handle both publishing `.s` DNS bridge records and obtaining a `.s` TLS certificate.

Set up `spagh-auto` per [the reference](./reference_spagh_auto.md).

You can use this [example config](TODO).

Once you've started it, you can visit the site at `https://IDENT.s` (with some assumptions: 1. you've set up DNS and the Certipasta root certificate, see [the guide to browsing](./guide_browse.md) 2. you're using IPv6 so there's no split horizon or else the server isn't in your local LAN, otherwise routing won't work).

## Setting up a reverse proxy

If you want to serve more complex software, the simplest way is to run that software over HTTP bound to `127.0.0.1` and then set up a spaghettinuum reverse proxy to expose it.

The `spagh-auto` command will do both of these for you.

Set up `spagh-auto` per [the reference](./reference_spagh_auto.md).

You can use this [example config](TODO).