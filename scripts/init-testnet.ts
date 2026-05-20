/**
 * Nebula DEX — Testnet Initialization
 * Initializes AMM configs for all 4 fee tiers on X1 testnet
 */

import * as anchor from "@coral-xyz/anchor";
import { Connection, Keypair, PublicKey } from "@solana/web3.js";
import fs from "fs";

const RPC = "https://rpc.testnet.x1.xyz";
const PROGRAM_ID = new PublicKey("23dn1qvEfhPfBVvm46PGWMRRr3rjE7QSitPkzEEbeCtQ");
const KEYPAIR_PATH = "/root/.openclaw/workspace/NBLAsmKbxKW9cwJy7cfAhWMY9HJSwMj87qRWy6E3YGY.json";

// Fee tiers: [index, tickSpacing, tradeFeeRate (per million), protocolFeeRate, fundFeeRate]
const FEE_TIERS = [
  { index: 0, tickSpacing: 1,   tradeFeeRate: 100,    label: "0.01%" },
  { index: 1, tickSpacing: 10,  tradeFeeRate: 500,    label: "0.05%" },
  { index: 2, tickSpacing: 60,  tradeFeeRate: 2500,   label: "0.25%" },
  { index: 3, tickSpacing: 200, tradeFeeRate: 10000,  label: "1.00%" },
];

const PROTOCOL_FEE_RATE = 120000; // 12% of trade fee
const FUND_FEE_RATE = 40000;      // 4% of trade fee

async function main() {
  const connection = new Connection(RPC, "confirmed");
  const keypairData = JSON.parse(fs.readFileSync(KEYPAIR_PATH, "utf-8"));
  const admin = Keypair.fromSecretKey(new Uint8Array(keypairData));

  console.log("=== Nebula DEX Testnet Init ===");
  console.log("Program:", PROGRAM_ID.toBase58());
  console.log("Admin:", admin.publicKey.toBase58());

  const bal = await connection.getBalance(admin.publicKey);
  console.log("Balance:", bal / 1e9, "XNT\n");

  const provider = new anchor.AnchorProvider(
    connection,
    new anchor.Wallet(admin),
    { commitment: "confirmed" }
  );
  anchor.setProvider(provider);

  // Load IDL
  const idlPath = "/root/.openclaw/workspace/projects/nebula-dex-fork/target/idl/nebula_dex.json";
  let idl: any;
  try {
    idl = JSON.parse(fs.readFileSync(idlPath, "utf-8"));
  } catch {
    console.error("IDL not found at", idlPath);
    console.log("Run: cargo build-sbf first to generate IDL");
    process.exit(1);
  }

  const program = new anchor.Program(idl, PROGRAM_ID, provider);

  for (const tier of FEE_TIERS) {
    console.log(`Initializing fee tier ${tier.label} (index ${tier.index}, tickSpacing ${tier.tickSpacing})...`);

    // Derive AMM config PDA
    const [ammConfig] = PublicKey.findProgramAddressSync(
      [Buffer.from("amm_config"), Buffer.from(new Uint16Array([tier.index]).buffer)],
      PROGRAM_ID
    );

    // Check if already exists
    const existing = await connection.getAccountInfo(ammConfig);
    if (existing) {
      console.log(`  Already exists: ${ammConfig.toBase58()} — skipping`);
      continue;
    }

    try {
      const tx = await (program.methods as any)
        .createAmmConfig(
          tier.index,
          tier.tickSpacing,
          tier.tradeFeeRate,
          PROTOCOL_FEE_RATE,
          FUND_FEE_RATE
        )
        .accounts({
          admin: admin.publicKey,
          ammConfig: ammConfig,
          systemProgram: anchor.web3.SystemProgram.programId,
        })
        .signers([admin])
        .rpc();

      console.log(`  ✅ Created: ${ammConfig.toBase58()}`);
      console.log(`  Tx: ${tx}`);
    } catch (err: any) {
      console.error(`  ❌ Failed: ${err.message}`);
    }
    console.log();
  }

  console.log("=== Init Complete ===");
  const finalBal = await connection.getBalance(admin.publicKey);
  console.log("Final balance:", finalBal / 1e9, "XNT");
}

main().catch(console.error);
