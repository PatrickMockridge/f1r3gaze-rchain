# The wallet

The agent driving the browser pays for work on the node. F1R3Gaze keeps the
user's wallets, and the **active wallet signs every deploy the browser
makes**. On this protocol the account charged for a deploy is its deployer, so
paying and signing are the same act.

## Keys

Wallet keys are secp256k1 keys kept in the profile's keystore under
`wallet:<address>`: the OS credential store on macOS and Windows (Keychain,
Credential Manager), a `0600` file per key on Linux. `wallets.tsv` in the
profile lists addresses and labels; `wallet-active` names the payer. Nothing
else writes a key anywhere.

Wallets are interchangeable with **F1R3Sky**: a wallet file is the Embers
SDK's format,

```
{"keyType":"secp256k1","value":"<64 hex digits, upper case>","valueFormat":"hex"}
```

and addresses are F1R3Cap addresses derived exactly as the SDK derives them.
The tests check addresses, files and even signature bytes against vectors
produced by running the SDK's own code.

```
f1r3gaze wallet new [LABEL]            create a wallet (the first becomes active)
f1r3gaze wallet import FILE [LABEL]    import a file F1R3Sky saved (or a hex key)
f1r3gaze wallet export ADDRESS [FILE]  write the wallet file (0600)
f1r3gaze wallet use ADDRESS            make ADDRESS the payer
f1r3gaze wallet list | balance [ADDRESS] | remove ADDRESS
f1r3gaze wallet send TO AMOUNT [NOTE]  transfer from the active wallet
```

The window has the same in its **Wallet** panel.

## What a page can make the wallet do

A page's program is deployed signed by the user's wallet, so the browser
limits what such a deploy can reach. Programs are rendered by the browser
from published code, and every system name they bind is checked against an
allow-list: registry lookup and insertion, standard output, the deploy's own
id, block data, the REV address and crypto functions. Everything else is
refused, and in particular **`rho:rchain:deployerId`**, the deployer's
identity, from which a program could obtain the vault's auth key and spend
the user's funds. A page can cost the user phlo, which the consent prompt
quotes along with the paying wallet and its balance; it cannot move funds.

Session messages are deploys too, paid by the wallet. A session is identified
to its service by a fresh session public key, as before; the session key no
longer signs.

Because one wallet signs for every site, sites can link a user's deploys by
the deployer key. Users who want separate identities can keep several
wallets and choose which pays.

## Transfers

Balances, history and transfers go through the **Embers** wallet API (the
service F1R3Sky uses; set `embers_api` in `settings.conf`). Embers prepares a
transfer contract; the browser does not sign what it cannot read, so before
signing it

1. decodes the prepared bytes strictly (only deploy-data fields, each once,
   canonical encoding: nothing hidden),
2. checks the shard and that `phlo_price × phlo_limit` is within `max_fee`,
3. requires the term to be exactly Embers' transfer template filled with the
   user's own from, to, amount and note (the server chooses only the
   environment URI and the timestamp).

The call carries no deployer identity, so a dishonest server can at worst
waste the capped fee. The tests run an honest mock Embers and a dishonest one
that swaps the recipient: nothing is signed or sent to the dishonest one.
