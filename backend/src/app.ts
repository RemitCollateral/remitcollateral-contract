import cors from "cors";
import express, { type Express } from "express";
import { createRouter } from "./routes";

interface AppOptions {
  adminApiKey?: string;
  corsOrigins?: string[];
}

function getCorsOrigins(): string[] {
  return (process.env.CORS_ORIGINS ?? "http://localhost:3000")
    .split(",")
    .map((origin) => origin.trim())
    .filter(Boolean);
}

export function createApp({ adminApiKey, corsOrigins = getCorsOrigins() }: AppOptions = {}): Express {
  const app = express();

  app.use(cors({ origin: corsOrigins }));
  app.use(express.json({ limit: "16kb" }));

  // Register API routes
  app.use("/api", createRouter({ adminApiKey }));

  // Default route
  app.get("/", (_req, res) => {
    res.send("Stellar Private Equity Platform Backend is running.");
  });

  return app;
}
