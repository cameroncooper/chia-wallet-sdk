/* tslint:disable */
/* eslint-disable */

/* auto-generated by NAPI-RS */

export interface Coin {
  parentCoinInfo: Uint8Array
  puzzleHash: Uint8Array
  amount: bigint
}
export declare function toCoinId(coin: Coin): Uint8Array
export interface CoinSpend {
  coin: Coin
  puzzleReveal: Uint8Array
  solution: Uint8Array
}
export interface LineageProof {
  parentParentCoinInfo: Uint8Array
  parentInnerPuzzleHash?: Uint8Array
  parentAmount: bigint
}
export interface Nft {
  coin: Coin
  lineageProof: LineageProof
  info: NftInfo
}
export interface NftInfo {
  launcherId: Uint8Array
  metadata: NftMetadata
  metadataUpdaterPuzzleHash: Uint8Array
  currentOwner?: Uint8Array
  royaltyPuzzleHash: Uint8Array
  royaltyTenThousandths: number
  p2PuzzleHash: Uint8Array
}
export interface NftMetadata {
  editionNumber: bigint
  editionTotal: bigint
  dataUris: Array<string>
  dataHash?: Uint8Array
  metadataUris: Array<string>
  metadataHash?: Uint8Array
  licenseUris: Array<string>
  licenseHash?: Uint8Array
}
export interface ParsedNft {
  nftInfo: NftInfo
  innerPuzzle: Uint8Array
}
export declare function parseNftInfo(puzzleReveal: Uint8Array): ParsedNft | null
export declare function parseUnspentNft(parentCoin: Coin, parentPuzzleReveal: Uint8Array, parentSolution: Uint8Array, coin: Coin): Nft | null
export interface NftMint {
  metadata: NftMetadata
  p2PuzzleHash: Uint8Array
  royaltyPuzzleHash: Uint8Array
  royaltyTenThousandths: number
}
export interface MintedNfts {
  nfts: Array<Nft>
  coinSpends: Array<CoinSpend>
  parentConditions: Array<Uint8Array>
}
export declare function mintNfts(parentCoinId: Uint8Array, nftMints: Array<NftMint>): MintedNfts
