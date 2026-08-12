import dotenv from "dotenv";
import { createApp } from "./app";

dotenv.config();

const app = createApp();
const PORT = process.env.PORT || 4000;

app.listen(PORT, () => {
  console.log(`=================================================`);
  console.log(`🚀 Server running on http://localhost:${PORT}`);
  console.log(`Network: ${process.env.STELLAR_NETWORK || "testnet"}`);
  console.log(`RPC: ${process.env.STELLAR_RPC_URL || "https://soroban-testnet.stellar.org"}`);
  console.log(`=================================================`);
});
