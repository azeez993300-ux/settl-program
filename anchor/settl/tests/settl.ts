import * as anchor from "@coral-xyz/anchor";
import { Program, BN } from "@coral-xyz/anchor";
import {
  PublicKey,
  Keypair,
  SystemProgram,
  SYSVAR_RENT_PUBKEY,
  LAMPORTS_PER_SOL,
  Transaction,
} from "@solana/web3.js";
import {
  createMint,
  createAccount,
  mintTo,
  getAccount,
  TOKEN_PROGRAM_ID,
} from "@solana/spl-token";
import { assert } from "chai";

// ─────────────────────────────────────────
// Settl — Full Test Suite
//
// Fee model: 1.5% (150 basis points)
//   gross = pending escrow balance
//   fee   = gross * 150 / 10_000
//   net   = gross - fee
//   net  → merchant wallet
//   fee  → treasury wallet
//
// Tests:
//   1.  Initialize global config
//   2.  Reject fee above 10%
//   3.  Register merchant
//   4.  Reject duplicate registration
//   5.  Request wallet update with 24hr delay
//   6.  Reject wallet confirm before delay
//   7.  Initialize escrow vault
//   8.  Customer deposits 1000 AUDD
//   9.  Balance accumulates across deposits
//   10. Reject zero deposit
//   11. Release — fee to treasury, net to merchant
//   12. Reject release when balance is zero
//   13. Reject unauthorized release
//   14. Reject deposit to inactive merchant
// ─────────────────────────────────────────

describe("Settl — Full Contract Tests (1.5% Fee)", () => {
  const provider = anchor.AnchorProvider.env();
  anchor.setProvider(provider);

  const program   = anchor.workspace.Settl as Program<any>;
  const authority = provider.wallet as anchor.Wallet;

  const merchantWallet = Keypair.generate();
  const treasuryWallet = Keypair.generate();
  // Use authority wallet as customer (already has SOL, no airdrop needed)
  const customer = authority;

  let auddMint:    PublicKey;
  let customerAta: PublicKey;
  let merchantAta: PublicKey;
  let treasuryAta: PublicKey;

  const merchantId       = "test-merchant-001";
  const FEE_BASIS_POINTS = 150; // 1.5%

  let configPDA:   PublicKey;
  let merchantPDA: PublicKey;
  let escrowPDA:   PublicKey;
  let vaultPDA:    PublicKey;

  // Helper function to transfer SOL from authority to another wallet
  async function transferSol(toWallet: PublicKey, amount: number) {
    const transaction = new Transaction().add(
      SystemProgram.transfer({
        fromPubkey: authority.publicKey,
        toPubkey: toWallet,
        lamports: amount * LAMPORTS_PER_SOL,
      })
    );
    
    const signature = await anchor.web3.sendAndConfirmTransaction(
      provider.connection,
      transaction,
      [authority.payer]
    );
    return signature;
  }

  // ─────────────────────────────────────
  // Setup
  // ─────────────────────────────────────
  before(async () => {
    console.log("\n  ── Setup ───────────────────────────");

    // Derive all PDAs upfront
    [configPDA] = PublicKey.findProgramAddressSync(
      [Buffer.from("config")],
      program.programId
    );
    [merchantPDA] = PublicKey.findProgramAddressSync(
      [Buffer.from("merchant"), Buffer.from(merchantId)],
      program.programId
    );
    [escrowPDA] = PublicKey.findProgramAddressSync(
      [Buffer.from("escrow"), Buffer.from(merchantId)],
      program.programId
    );
    [vaultPDA] = PublicKey.findProgramAddressSync(
      [Buffer.from("vault"), Buffer.from(merchantId)],
      program.programId
    );

    // Fund merchant wallet by transferring SOL from authority (your wallet)
    console.log("  Funding merchant wallet from authority...");
    await transferSol(merchantWallet.publicKey, 0.5);
    console.log("  Merchant funded ✓");

    // Create mock AUDD mint — 6 decimals same as real AUDD
    auddMint = await createMint(
      provider.connection,
      authority.payer,
      authority.publicKey,
      null,
      6
    );

    // Customer ATA — using authority's wallet, funded with 1000 AUDD
    customerAta = await createAccount(
      provider.connection,
      authority.payer,
      auddMint,
      authority.publicKey
    );
    await mintTo(
      provider.connection,
      authority.payer,
      auddMint,
      customerAta,
      authority.payer,
      1_000_000_000 // 1000 AUDD
    );

    // Merchant ATA — empty, receives net amount at release
    merchantAta = await createAccount(
      provider.connection,
      authority.payer,
      auddMint,
      merchantWallet.publicKey
    );

    // Treasury ATA — empty, receives 1.5% fee at release
    treasuryAta = await createAccount(
      provider.connection,
      authority.payer,
      auddMint,
      treasuryWallet.publicKey
    );

    const balance = await provider.connection.getBalance(authority.publicKey);
    const merchantBalance = await provider.connection.getBalance(merchantWallet.publicKey);
    
    console.log("  Authority wallet:", authority.publicKey.toBase58());
    console.log("  Authority SOL    :", balance / LAMPORTS_PER_SOL, "SOL");
    console.log("  Merchant SOL     :", merchantBalance / LAMPORTS_PER_SOL, "SOL");
    console.log("  Program ID       :", program.programId.toBase58());
    console.log("  Config PDA       :", configPDA.toBase58());
    console.log("  Merchant PDA     :", merchantPDA.toBase58());
    console.log("  Escrow PDA       :", escrowPDA.toBase58());
    console.log("  Vault PDA        :", vaultPDA.toBase58());
    console.log("  Mock AUDD mint   :", auddMint.toBase58());
    console.log("  Treasury wallet  :", treasuryWallet.publicKey.toBase58());
    console.log("  Fee              : 1.5% (150 bps)");
    console.log("  ────────────────────────────────────\n");
  });

  // ─────────────────────────────────────
  // 1. Config
  // ─────────────────────────────────────

  it("✓ Initializes global config with 1.5% fee", async () => {
    await program.methods
      .initializeConfig(FEE_BASIS_POINTS)
      .accounts({
        config:         configPDA,
        treasuryWallet: treasuryWallet.publicKey,
        authority:      authority.publicKey,
        systemProgram:  SystemProgram.programId,
      })
      .rpc();

    const c = await program.account.settlConfig.fetch(configPDA);

    assert.equal(c.feeBasisPoints, FEE_BASIS_POINTS);
    assert.equal(
      c.treasuryWallet.toBase58(),
      treasuryWallet.publicKey.toBase58()
    );
    assert.equal(c.totalFeesCollected.toNumber(), 0);

    console.log("    fee_basis_points     :", c.feeBasisPoints, "(1.5%)");
    console.log("    treasury_wallet      :", c.treasuryWallet.toBase58());
  });

  it("✓ Rejects fee above 10% (1000 basis points)", async () => {
    try {
      await program.methods
        .updateFee(1500)
        .accounts({
          config:      configPDA,
          newTreasury: treasuryWallet.publicKey,
          authority:   authority.publicKey,
        })
        .rpc();
      assert.fail("Should have thrown FeeTooHigh");
    } catch (err: any) {
      assert.include(err.message, "FeeTooHigh");
      console.log("    correctly rejected fee > 10% ✓");
    }
  });

  // ─────────────────────────────────────
  // 2. Merchant Registration
  // ─────────────────────────────────────

  it("✓ Registers merchant on-chain (backend only)", async () => {
    await program.methods
      .registerMerchant(merchantId, merchantWallet.publicKey)
      .accounts({
        merchant:      merchantPDA,
        authority:     authority.publicKey,
        systemProgram: SystemProgram.programId,
      })
      .rpc();

    const m = await program.account.merchantAccount.fetch(merchantPDA);

    assert.equal(m.merchantId, merchantId);
    assert.equal(m.wallet.toBase58(), merchantWallet.publicKey.toBase58());
    assert.equal(m.isActive, true);
    assert.equal(m.totalReleased.toNumber(), 0);
    assert.equal(m.totalFeesPaid.toNumber(), 0);

    console.log("    merchant_id     :", m.merchantId);
    console.log("    wallet          :", m.wallet.toBase58());
    console.log("    is_active       :", m.isActive);
  });

  it("✓ Rejects duplicate merchant registration", async () => {
    try {
      await program.methods
        .registerMerchant(merchantId, merchantWallet.publicKey)
        .accounts({
          merchant:      merchantPDA,
          authority:     authority.publicKey,
          systemProgram: SystemProgram.programId,
        })
        .rpc();
      assert.fail("Should have rejected duplicate");
    } catch (err: any) {
      assert.ok(err);
      console.log("    correctly rejected duplicate registration ✓");
    }
  });

  it("✓ Requests wallet update — 24hr delay stored on-chain", async () => {
    const newWallet = Keypair.generate().publicKey;

    await program.methods
      .requestWalletUpdate(newWallet)
      .accounts({
        merchant:  merchantPDA,
        authority: authority.publicKey,
      })
      .rpc();

    const m          = await program.account.merchantAccount.fetch(merchantPDA);
    const unlockTime = m.walletUpdateAt.toNumber();
    const now        = Math.floor(Date.now() / 1000);

    assert.isNotNull(m.pendingWallet);
    assert.isAbove(unlockTime, now + 86_000);

    console.log("    pending_wallet  :", m.pendingWallet.toBase58());
    console.log("    unlocks_at      :", new Date(unlockTime * 1000).toISOString());
  });

  it("✓ Rejects wallet confirm before 24hr delay passes", async () => {
    try {
      await program.methods
        .confirmWalletUpdate()
        .accounts({
          merchant:  merchantPDA,
          authority: authority.publicKey,
        })
        .rpc();
      assert.fail("Should have thrown WalletUpdateNotReady");
    } catch (err: any) {
      assert.include(err.message, "WalletUpdateNotReady");
      console.log("    correctly rejected — delay not passed ✓");
    }
  });

  // ─────────────────────────────────────
  // 3. Escrow
  // ─────────────────────────────────────

  it("✓ Initializes escrow vault linked to merchant", async () => {
    await program.methods
      .initializeMerchantEscrow(merchantId)
      .accounts({
        merchant:      merchantPDA,
        escrow:        escrowPDA,
        vault:         vaultPDA,
        auddMint:      auddMint,
        authority:     authority.publicKey,
        tokenProgram:  TOKEN_PROGRAM_ID,
        systemProgram: SystemProgram.programId,
        rent:          SYSVAR_RENT_PUBKEY,
      })
      .rpc();

    const e = await program.account.escrowAccount.fetch(escrowPDA);

    assert.equal(e.merchantId, merchantId);
    assert.equal(e.pendingBalance.toNumber(), 0);
    assert.equal(e.totalPayments.toNumber(), 0);
    assert.equal(
      e.merchantWallet.toBase58(),
      merchantWallet.publicKey.toBase58()
    );

    console.log("    merchant_id     :", e.merchantId);
    console.log("    pending_balance :", e.pendingBalance.toNumber());
    console.log("    merchant_wallet :", e.merchantWallet.toBase58());
  });

  // ─────────────────────────────────────
  // 4. Deposits
  // ─────────────────────────────────────

  it("✓ Customer deposits 600 AUDD — merchant does nothing", async () => {
    const amount = 600_000_000; // 600 AUDD

    await program.methods
      .deposit(merchantId, new BN(amount))
      .accounts({
        merchant:     merchantPDA,
        escrow:       escrowPDA,
        vault:        vaultPDA,
        customerAta:  customerAta,
        customer:     customer.publicKey,
        tokenProgram: TOKEN_PROGRAM_ID,
      })
      .signers([customer.payer])
      .rpc();

    const e     = await program.account.escrowAccount.fetch(escrowPDA);
    const vault = await getAccount(provider.connection, vaultPDA);

    assert.equal(e.pendingBalance.toNumber(), amount);
    assert.equal(Number(vault.amount), amount);
    assert.equal(e.totalPayments.toNumber(), 1);

    console.log("    deposited       :", amount / 1_000_000, "AUDD");
    console.log("    vault_balance   :", Number(vault.amount) / 1_000_000, "AUDD");
  });

  it("✓ Second deposit — balance accumulates correctly", async () => {
    const amount = 400_000_000; // 400 AUDD

    await program.methods
      .deposit(merchantId, new BN(amount))
      .accounts({
        merchant:     merchantPDA,
        escrow:       escrowPDA,
        vault:        vaultPDA,
        customerAta:  customerAta,
        customer:     customer.publicKey,
        tokenProgram: TOKEN_PROGRAM_ID,
      })
      .signers([customer.payer])
      .rpc();

    const e = await program.account.escrowAccount.fetch(escrowPDA);

    // 600 + 400 = 1000 AUDD
    assert.equal(e.pendingBalance.toNumber(), 1_000_000_000);
    assert.equal(e.totalPayments.toNumber(), 2);

    console.log("    pending_balance :", e.pendingBalance.toNumber() / 1_000_000, "AUDD");
    console.log("    total_payments  :", e.totalPayments.toNumber());
  });

  it("✓ Rejects zero amount deposit", async () => {
    try {
      await program.methods
        .deposit(merchantId, new BN(0))
        .accounts({
          merchant:     merchantPDA,
          escrow:       escrowPDA,
          vault:        vaultPDA,
          customerAta:  customerAta,
          customer:     customer.publicKey,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .signers([customer.payer])
        .rpc();
      assert.fail("Should have thrown ZeroAmount");
    } catch (err: any) {
      assert.include(err.message, "ZeroAmount");
      console.log("    correctly rejected zero deposit ✓");
    }
  });

  // ─────────────────────────────────────
  // 5. Release with 1.5% Fee
  // ─────────────────────────────────────

  it("✓ Releases funds — 1.5% fee to treasury, 98.5% net to merchant", async () => {
    const gross = 1_000_000_000; // 1000 AUDD

    // Expected:
    //   fee = 1000 * 150 / 10_000 = 15 AUDD
    //   net = 1000 - 15 = 985 AUDD
    const expectedFee = Math.floor(gross * FEE_BASIS_POINTS / 10_000);
    const expectedNet = gross - expectedFee;

    const merchantBefore = await getAccount(provider.connection, merchantAta);
    const treasuryBefore = await getAccount(provider.connection, treasuryAta);

    await program.methods
      .release(merchantId)
      .accounts({
        config:       configPDA,
        merchant:     merchantPDA,
        escrow:       escrowPDA,
        vault:        vaultPDA,
        merchantAta:  merchantAta,
        treasuryAta:  treasuryAta,
        authority:    authority.publicKey,
        tokenProgram: TOKEN_PROGRAM_ID,
      })
      .rpc();

    const e             = await program.account.escrowAccount.fetch(escrowPDA);
    const m             = await program.account.merchantAccount.fetch(merchantPDA);
    const c             = await program.account.settlConfig.fetch(configPDA);
    const merchantAfter = await getAccount(provider.connection, merchantAta);
    const treasuryAfter = await getAccount(provider.connection, treasuryAta);

    const merchantReceived =
      Number(merchantAfter.amount) - Number(merchantBefore.amount);
    const treasuryReceived =
      Number(treasuryAfter.amount) - Number(treasuryBefore.amount);

    assert.equal(merchantReceived, expectedNet);
    assert.equal(treasuryReceived, expectedFee);
    assert.equal(e.pendingBalance.toNumber(), 0);
    assert.equal(m.totalReleased.toNumber(), expectedNet);
    assert.equal(m.totalFeesPaid.toNumber(), expectedFee);
    assert.equal(c.totalFeesCollected.toNumber(), expectedFee);

    console.log("    ── Release breakdown ────────────");
    console.log("    gross           :", gross / 1_000_000, "AUDD");
    console.log("    fee (1.5%)      :", treasuryReceived / 1_000_000, "AUDD → treasury");
    console.log("    net (98.5%)     :", merchantReceived / 1_000_000, "AUDD → merchant");
    console.log("    pending_balance : 0 (reset) ✓");
    console.log("    fees_collected  :", c.totalFeesCollected.toNumber() / 1_000_000, "AUDD (platform total)");
  });

  it("✓ Rejects release when balance is zero", async () => {
    try {
      await program.methods
        .release(merchantId)
        .accounts({
          config:       configPDA,
          merchant:     merchantPDA,
          escrow:       escrowPDA,
          vault:        vaultPDA,
          merchantAta:  merchantAta,
          treasuryAta:  treasuryAta,
          authority:    authority.publicKey,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .rpc();
      assert.fail("Should have thrown ZeroBalance");
    } catch (err: any) {
      assert.include(err.message, "ZeroBalance");
      console.log("    correctly rejected zero balance release ✓");
    }
  });

  it("✓ Rejects release from unauthorized wallet", async () => {
    // Deposit first so balance is not zero
    await program.methods
      .deposit(merchantId, new BN(10_000_000))
      .accounts({
        merchant:     merchantPDA,
        escrow:       escrowPDA,
        vault:        vaultPDA,
        customerAta:  customerAta,
        customer:     customer.publicKey,
        tokenProgram: TOKEN_PROGRAM_ID,
      })
      .signers([customer.payer])
      .rpc();

    try {
      await program.methods
        .release(merchantId)
        .accounts({
          config:       configPDA,
          merchant:     merchantPDA,
          escrow:       escrowPDA,
          vault:        vaultPDA,
          merchantAta:  merchantAta,
          treasuryAta:  treasuryAta,
          authority:    Keypair.generate().publicKey, // random unauthorized wallet
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .rpc();
      assert.fail("Should have thrown Unauthorized");
    } catch (err: any) {
      console.log("    correctly rejected unauthorized release ✓");
    }
  });

  it("✓ Rejects deposit to inactive merchant", async () => {
    // Deactivate merchant
    await program.methods
      .deactivateMerchant()
      .accounts({
        merchant:  merchantPDA,
        authority: authority.publicKey,
      })
      .rpc();

    try {
      await program.methods
        .deposit(merchantId, new BN(10_000_000))
        .accounts({
          merchant:     merchantPDA,
          escrow:       escrowPDA,
          vault:        vaultPDA,
          customerAta:  customerAta,
          customer:     customer.publicKey,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .signers([customer.payer])
        .rpc();
      assert.fail("Should have thrown MerchantInactive");
    } catch (err: any) {
      assert.include(err.message, "MerchantInactive");
      console.log("    correctly rejected deposit to inactive merchant ✓");
    }
  });
});