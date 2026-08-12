import assert from "node:assert/strict";
import { createServer } from "node:http";
import test from "node:test";
import type { Express } from "express";
import { createApp } from "./app";

async function withServer(app: Express, run: (baseUrl: string) => Promise<void>): Promise<void> {
  const server = createServer(app);

  await new Promise<void>((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolve);
  });

  const address = server.address();
  assert.ok(address && typeof address !== "string");

  try {
    await run(`http://127.0.0.1:${address.port}`);
  } finally {
    await new Promise<void>((resolve, reject) => {
      server.close((error) => (error ? reject(error) : resolve()));
    });
  }
}

test("protects KYC administration and keeps document data private", async () => {
  await withServer(createApp({ adminApiKey: "test-admin-key" }), async (baseUrl) => {
    const investorAddress = "GTESTKYCINVESTOR001";
    const kycSubmission = {
      investorAddress,
      fullName: "Ada Lovelace",
      documentType: "Passport",
      documentNumber: "123456789",
    };

    const submitted = await fetch(`${baseUrl}/api/kyc/submit`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(kycSubmission),
    });
    assert.equal(submitted.status, 201);
    const submittedBody = (await submitted.json()) as {
      submission: { status: string; fullName?: string; documentNumber?: string };
    };
    assert.equal(submittedBody.submission.status, "PENDING");
    assert.equal("fullName" in submittedBody.submission, false);
    assert.equal("documentNumber" in submittedBody.submission, false);

    const publicStatus = await fetch(`${baseUrl}/api/investor/${investorAddress}`);
    assert.equal(publicStatus.status, 200);
    const publicStatusBody = (await publicStatus.json()) as {
      status: string;
      fullName?: string;
      documentNumber?: string;
    };
    assert.equal(publicStatusBody.status, "PENDING");
    assert.equal("fullName" in publicStatusBody, false);
    assert.equal("documentNumber" in publicStatusBody, false);

    const unauthenticatedPending = await fetch(`${baseUrl}/api/kyc/pending`);
    assert.equal(unauthenticatedPending.status, 401);

    const unauthenticatedApproval = await fetch(`${baseUrl}/api/kyc/approve`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ investorAddress }),
    });
    assert.equal(unauthenticatedApproval.status, 401);

    const authenticatedPending = await fetch(`${baseUrl}/api/kyc/pending`, {
      headers: { "x-admin-api-key": "test-admin-key" },
    });
    assert.equal(authenticatedPending.status, 200);
    const pendingBody = (await authenticatedPending.json()) as Array<{ documentNumber: string }>;
    assert.equal(pendingBody.length, 1);
    assert.equal(pendingBody[0].documentNumber, "123456789");
  });
});

test("fails closed when the admin API key is not configured", async () => {
  await withServer(createApp({ adminApiKey: "" }), async (baseUrl) => {
    const pending = await fetch(`${baseUrl}/api/kyc/pending`, {
      headers: { "x-admin-api-key": "any-key" },
    });

    assert.equal(pending.status, 503);
    assert.deepEqual(await pending.json(), { error: "KYC admin API is not configured" });
  });
});
