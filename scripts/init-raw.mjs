/**
 * Nebula DEX — Raw Testnet Init (no IDL needed)
 * Sends createAmmConfig instruction directly
 */

import { Connection, Keypair, PublicKey, Transaction, TransactionInstruction, SystemProgram, LAMPORTS_PER_SOL } from "@solana/web3.js";
import { createHash } from "crypto";
import fs from "fs";

const RPC = "https://rpc.testnet.x1.xyz";
const PROGRAM_ID = new PublicKey("23dn1qvEfhPfBVvm46PGWMRRr3rjE7QSitPkzEEbeCtQ");
const KEYPAIR_PATH = "/root/.openclaw/workspace/NBLAsmKbxKW9cwJy7cfAhWMY9HJSwMj87qRWy6E3YGY.json";

// Anchor discriminator = first 8 bytes of sha256("global:<instruction_name>")
function discriminator(name) {
  const hash = createHash("sha256").update(`global:${name}`).digest();
  return hash.slice(0, 8);
}

// Encode u16 little-endian
function u16LE(n) {
  const buf = Buffer.alloc(2);
  buf.writeUInt16LE(n, 0);
  return buf;
}

// Encode u32 little-endian
function u32LE(n) {
  const buf = Buffer.alloc(4);
  buf.writeUInt32LE(n, 0);
  return buf;
}

const FEE_TIERS = [
  { index: 0, tickSpacing: 1,   tradeFeeRate: 100,   label: "0.01%" },
  { index: 1, tickSpacing: 10,  tradeFeeRate: 500,   label: "0.05%" },
  { index: 2, tickSpacing: 60,  tradeFeeRate: 2500,  label: "0.25%" },
  { index: 3, tickSpacing: 200, tradeFeeRate: 10000, label: "1.00%" },
];

const PROTOCOL_FEE_RATE = 120000;
const FUND_FEE_RATE = 40000;

async function main() {
  const connection = new Connection(RPC, "confirmed");
  const keypairData = JSON.parse(fs.readFileSync(KEYPAIR_PATH, "utf-8"));
  const admin = Keypair.fromSecretKey(new Uint8Array(keypairData));

  console.log("=== Nebula DEX Testnet AMM Init ===");
  console.log("Program:", PROGRAM_ID.toBase58());
  console.log("Admin:", admin.publicKey.toBase58());
  const bal = await connection.getBalance(admin.publicKey);
  console.log("Balance:", (bal / LAMPORTS_PER_SOL).toFixed(4), "XNT\n");

  const disc = discriminator("create_amm_config");
  console.log("Instruction discriminator:", disc.toString("hex"));

  for (const tier of FEE_TIERS) {
    console.log(`\nInitializing fee tier ${tier.label} (index ${tier.index})...`);

    // AMM config PDA: seeds = ["amm_config", index_as_u16_BE]
    const indexBE = Buffer.alloc(2);
    indexBE.writeUInt16BE(tier.index, 0);
    const [ammConfig, bump] = PublicKey.findProgramAddressSync(
      [Buffer.from("amm_config"), indexBE],
      PROGRAM_ID
    );
    console.log("  AMM Config PDA:", ammConfig.toBase58());

    // Check if already exists
    const existing = await connection.getAccountInfo(ammConfig);
    if (existing) {
      console.log("  Already exists — skipping");
      continue;
    }

    // Build instruction data: discriminator + index(u16) + tickSpacing(u16) + tradeFee(u32) + protocolFee(u32) + fundFee(u32)
    const data = Buffer.concat([
      disc,
      u16LE(tier.index),
      u16LE(tier.tickSpacing),
      u32LE(tier.tradeFeeRate),
      u32LE(PROTOCOL_FEE_RATE),
      u32LE(FUND_FEE_RATE),
    ]);

    const ix = new TransactionInstruction({
      programId: PROGRAM_ID,
      keys: [
        { pubkey: admin.publicKey, isSigner: true, isWritable: true },  // owner (must match crate::admin::ID)
        { pubkey: ammConfig,       isSigner: false, isWritable: true },  // amm_config PDA
        { pubkey: SystemProgram.programId, isSigner: false, isWritable: false },
      ],
      data,
    });

    try {
      const { blockhash } = await connection.getLatestBlockhash();
      const tx = new Transaction({ recentBlockhash: blockhash, feePayer: admin.publicKey });
      tx.add(ix);
      tx.sign(admin);

      const sig = await connection.sendRawTransaction(tx.serialize(), { skipPreflight: false });
      await connection.confirmTransaction(sig, "confirmed");
      console.log("  ✅ Created!");
      console.log("  Tx:", sig);
    } catch (err) {
      console.error("  ❌ Error:", err.message);
      if (err.logs) console.error("  Logs:", err.logs.slice(-5).join("\n"));
    }
  }

  const finalBal = await connection.getBalance(admin.publicKey);
  console.log("\n=== Init Complete ===");
  console.log("Final balance:", (finalBal / LAMPORTS_PER_SOL).toFixed(4), "XNT");
}

main().catch(console.error);
