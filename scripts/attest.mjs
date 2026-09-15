#!/usr/bin/env node
// Submit a repayment attestation co-signed by the loan's partner and a verifier.
//
//   node scripts/attest.mjs <ledger-contract-id> <loan-id> <amount>
//
// Environment:
//   PARTNER_SECRET, VERIFIER_SECRET   secret keys of the two signers
//   RPC_URL                           default https://soroban-testnet.stellar.org
//   NETWORK_PASSPHRASE                default the testnet passphrase
//
// The verifier builds and sends the transaction, so its signature on the
// envelope covers its own authorization. The partner is a second address, so
// it signs its own authorization entry. This is the same flow the backend uses
// to co-sign a partner's repayment report in production.
import { Keypair, Networks, contract, rpc } from '@stellar/stellar-sdk';
import dns from 'node:dns';
import net from 'node:net';

// Connect over IPv4 only. The RPC host also publishes an IPv6 address, and
// Node races the two with a 250 ms per-attempt timeout: when the first IPv4
// connection is slow it falls through to IPv6, and on a machine with no IPv6
// route the request fails outright. Turning the race off removes that.
dns.setDefaultResultOrder('ipv4first');
net.setDefaultAutoSelectFamily(false);

const [ledgerId, loanId, amount] = process.argv.slice(2);
if (!ledgerId || !loanId || !amount) {
  console.error('usage: attest.mjs <ledger-contract-id> <loan-id> <amount>');
  process.exit(2);
}
const RPC_URL = process.env.RPC_URL ?? 'https://soroban-testnet.stellar.org';
const PASSPHRASE = process.env.NETWORK_PASSPHRASE ?? Networks.TESTNET;

let step = 'reading keys';
try {
  const partner = Keypair.fromSecret(process.env.PARTNER_SECRET);
  const verifier = Keypair.fromSecret(process.env.VERIFIER_SECRET);

  step = 'loading the contract spec';
  const client = await contract.Client.from({
    contractId: ledgerId,
    rpcUrl: RPC_URL,
    networkPassphrase: PASSPHRASE,
    publicKey: verifier.publicKey(),
    ...contract.basicNodeSigner(verifier, PASSPHRASE),
  });

  step = 'simulating the attestation';
  // Builds and simulates. A refused attestation (wrong partner, unknown
  // verifier, overpayment) fails here, before anything is signed.
  const tx = await client.attest_repayment({
    partner: partner.publicKey(),
    verifier: verifier.publicKey(),
    loan_id: BigInt(loanId),
    amount_usd: BigInt(amount),
  });
  // A rejected simulation does not throw; surface the contract's own error
  // (e.g. Error(Contract, #3)) rather than failing later for a vaguer reason.
  if (rpc.Api.isSimulationError(tx.simulation)) {
    throw new Error(tx.simulation.error);
  }

  step = 'collecting the partner signature';
  // The partner must be the only other signer the contract asked for.
  const needed = await tx.needsNonInvokerSigningBy();
  if (needed.length !== 1 || needed[0] !== partner.publicKey()) {
    throw new Error(`unexpected signers required: ${needed.join(', ') || 'none'}`);
  }
  await tx.signAuthEntries({
    address: partner.publicKey(),
    signAuthEntry: contract.basicNodeSigner(partner, PASSPHRASE).signAuthEntry,
  });

  step = 'sending';
  const { result } = await tx.signAndSend();
  console.log(String(result));
} catch (err) {
  const cause = err?.cause ? ` (${err.cause.code ?? ''} ${err.cause.message ?? err.cause})` : '';
  console.error(`attestation failed while ${step}: ${err?.message ?? err}${cause}`);
  process.exit(1);
}
