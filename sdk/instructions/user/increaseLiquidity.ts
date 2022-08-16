import { PublicKey, TransactionInstruction } from "@solana/web3.js";
import { Program, BN } from "@project-serum/anchor";
import { AmmV3 } from "../../anchor/amm_v3";


export function increaseLiquidityInstruction(
  program: Program<AmmV3>,
  args: {
    liquidity: BN;
    amount0Min: BN;
    amount1Min: BN;
  },
  accounts:  {
    nftOwner: PublicKey;
    nftAccount: PublicKey;
    poolState: PublicKey;
    protocolPosition: PublicKey;
    personalPosition: PublicKey;
    tickArrayLower: PublicKey;
    tickArrayUpper: PublicKey;
    tokenAccount0: PublicKey;
    tokenAccount1: PublicKey;
    tokenVault0: PublicKey;
    tokenVault1: PublicKey;
    tokenProgram: PublicKey;
  }
): Promise<TransactionInstruction> {
  const {liquidity, amount0Min, amount1Min } = args;

  return program.methods
    .increaseLiquidity(liquidity, amount0Min, amount1Min)
    .accounts(accounts)
    .remainingAccounts([])
    .instruction();
}
