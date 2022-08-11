#!/usr/bin/env ts-node

import { Connection, PublicKey, Keypair } from "@solana/web3.js";
import { Context, NodeWallet } from "../base";
import { StateFetcher } from "../states";
import { sendTransaction } from "../utils";
import { AmmInstruction } from "../instructions";
import { Config, defaultConfirmOptions } from "./config";
import { AmmPool } from "../pool";
import keypairFile from "./owner-keypair.json";
import {
  Token,
  ASSOCIATED_TOKEN_PROGRAM_ID,
  TOKEN_PROGRAM_ID,
} from "@solana/spl-token";
import { LiquidityMath, SqrtPriceMath } from "../math";
async function main() {
  const owner = Keypair.fromSeed(Uint8Array.from(keypairFile.slice(0, 32)));
  const connection = new Connection(
    Config.url,
    defaultConfirmOptions.commitment
  );
  const ctx = new Context(
    connection,
    NodeWallet.fromSecretKey(owner),
    Config.programId,
    defaultConfirmOptions
  );
  const stateFetcher = new StateFetcher(ctx.program);
  const params = Config["decrease-liquidity"];
  for (let i = 0; i < params.length; i++) {
    const param = params[i];

    const poolStateData = await stateFetcher.getPoolState(
      new PublicKey(param.poolId)
    );
    const token0Account = await Token.getAssociatedTokenAddress(
      ASSOCIATED_TOKEN_PROGRAM_ID,
      TOKEN_PROGRAM_ID,
      poolStateData.tokenMint0,
      owner.publicKey
    );
    const token1Account = await Token.getAssociatedTokenAddress(
      ASSOCIATED_TOKEN_PROGRAM_ID,
      TOKEN_PROGRAM_ID,
      poolStateData.tokenMint1,
      owner.publicKey
    );
    const ammConfigData = await stateFetcher.getAmmConfig(
      new PublicKey(poolStateData.ammConfig)
    );
    const ammPool = new AmmPool(
      ctx,
      new PublicKey(param.poolId),
      poolStateData,
      ammConfigData,
      stateFetcher
    );

    const personalPositionData =
      await ammPool.stateFetcher.getPersonalPositionState(
        new PublicKey(param.positionId)
      );
    const priceLower = SqrtPriceMath.getSqrtPriceX64FromTick(
      personalPositionData.tickLowerIndex
    );
    console.log(
      "personalPositionData.tickLowerIndex:",
      personalPositionData.tickLowerIndex,
      "priceLower:",
      priceLower.toString()
    );
    const priceUpper = SqrtPriceMath.getSqrtPriceX64FromTick(
      personalPositionData.tickUpperIndex
    );
    console.log(
      "personalPositionData.tickUpperIndex:",
      personalPositionData.tickUpperIndex,
      "tickUpperIndex:",
      priceLower.toString()
    );
    const liquidity = LiquidityMath.getLiquidityFromTokenAmounts(
      poolStateData.sqrtPriceX64,
      priceLower,
      priceUpper,
      param.token0Amount,
      param.token1Amount
    );

    const [token0Amount, token1Amount] = LiquidityMath.getAmountsFromLiquidity(
      poolStateData.sqrtPriceX64,
      priceLower,
      priceUpper,
      liquidity
    );
    const ix = await AmmInstruction.decreaseLiquidityWithInputAmount(
      {
        positionNftOwner: owner.publicKey,
        token0Account,
        token1Account,
      },
      ammPool,
      personalPositionData,
      token0Amount,
      token1Amount,
      param.amountSlippage
    );
    let tx = await sendTransaction(
      ctx.connection,
      [ix],
      [owner],
      defaultConfirmOptions
    );
    console.log("decreaseLiquidity tx: ", tx);
  }
}

main();
