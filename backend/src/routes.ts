import { timingSafeEqual } from "node:crypto";
import { Request, Response, Router } from "express";
import { whitelistInvestorOnChain } from "./stellar";

// Mock Database for KYC submissions
export interface KycSubmission {
  investorAddress: string;
  fullName: string;
  documentType: string;
  documentNumber: string;
  status: "PENDING" | "APPROVED" | "REJECTED";
  txHash?: string;
  submittedAt: Date;
}

interface PublicKycStatus {
  investorAddress: string;
  status: KycSubmission["status"] | "NONE";
  submittedAt?: Date;
  txHash?: string;
}

interface RouterOptions {
  adminApiKey?: string;
}

const kycDb: Map<string, KycSubmission> = new Map();

function isNonEmptyString(value: unknown): value is string {
  return typeof value === "string" && value.trim().length > 0;
}

function toPublicKycStatus(submission?: KycSubmission): PublicKycStatus {
  if (!submission) {
    return { investorAddress: "", status: "NONE" };
  }

  return {
    investorAddress: submission.investorAddress,
    status: submission.status,
    submittedAt: submission.submittedAt,
    txHash: submission.txHash,
  };
}

function isAuthorized(request: Request, adminApiKey: string): boolean {
  const providedApiKey = request.header("x-admin-api-key");
  if (!providedApiKey) {
    return false;
  }

  const expected = Buffer.from(adminApiKey);
  const provided = Buffer.from(providedApiKey);

  return expected.length === provided.length && timingSafeEqual(expected, provided);
}

export function createRouter({ adminApiKey = process.env.ADMIN_API_KEY }: RouterOptions = {}): Router {
  const router = Router();

  const requireAdmin = (req: Request, res: Response, next: () => void) => {
    if (!adminApiKey) {
      return res.status(503).json({ error: "KYC admin API is not configured" });
    }

    if (!isAuthorized(req, adminApiKey)) {
      return res.status(401).json({ error: "Unauthorized" });
    }

    return next();
  };

  // Health Check
  router.get("/health", (_req: Request, res: Response) => {
    res.json({ status: "ok", timestamp: new Date() });
  });

  // 1. Submit KYC (LP action)
  router.post("/kyc/submit", (req: Request, res: Response) => {
    const { investorAddress, fullName, documentType, documentNumber } = req.body;

    if (
      !isNonEmptyString(investorAddress) ||
      !isNonEmptyString(fullName) ||
      !isNonEmptyString(documentType) ||
      !isNonEmptyString(documentNumber)
    ) {
      return res.status(400).json({ error: "Missing or invalid required fields" });
    }

    const submission: KycSubmission = {
      investorAddress: investorAddress.trim(),
      fullName: fullName.trim(),
      documentType: documentType.trim(),
      documentNumber: documentNumber.trim(),
      status: "PENDING",
      submittedAt: new Date(),
    };

    kycDb.set(submission.investorAddress, submission);
    console.log(`KYC submitted for investor: ${submission.investorAddress}`);

    return res.status(201).json({
      message: "KYC submitted successfully",
      submission: toPublicKycStatus(submission),
    });
  });

  // 2. Get Pending KYC Submissions (GP action)
  router.get("/kyc/pending", requireAdmin, (_req: Request, res: Response) => {
    const pending = Array.from(kycDb.values()).filter((s) => s.status === "PENDING");
    return res.json(pending);
  });

  // 3. Approve KYC & Trigger Whitelisting (GP action)
  router.post("/kyc/approve", requireAdmin, async (req: Request, res: Response) => {
    const { investorAddress } = req.body;

    if (!isNonEmptyString(investorAddress)) {
      return res.status(400).json({ error: "Missing or invalid investor address" });
    }

    const submission = kycDb.get(investorAddress.trim());
    if (!submission) {
      return res.status(404).json({ error: "KYC submission not found" });
    }

    if (submission.status === "APPROVED") {
      return res.status(400).json({ error: "Investor is already approved" });
    }

    try {
      // Trigger on-chain whitelisting via the GP's key
      const txHash = await whitelistInvestorOnChain(submission.investorAddress);

      submission.status = "APPROVED";
      submission.txHash = txHash;
      kycDb.set(submission.investorAddress, submission);

      console.log(`Successfully whitelisted investor ${submission.investorAddress}. Tx: ${txHash}`);

      return res.json({
        message: "KYC approved and investor whitelisted on-chain",
        txHash,
        submission,
      });
    } catch (error: unknown) {
      console.error("Error whitelisting investor:", error);
      return res.status(500).json({ error: "Failed to whitelist investor on-chain" });
    }
  });

  // 4. Get Investor KYC Status
  router.get("/investor/:address", (req: Request, res: Response) => {
    const address = req.params.address.trim();
    const submission = kycDb.get(address);
    const status = toPublicKycStatus(submission);

    return res.json({ ...status, investorAddress: address });
  });

  return router;
}
