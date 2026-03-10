const { ethers } = require("ethers");
require("dotenv").config({ path: "/root/bot_enhanced/.env" });

async function main() {
    const provider = new ethers.providers.JsonRpcProvider("http://127.0.0.1:9650/ext/bc/C/rpc");
    
    // Bot 3 is index 2 in PRIVATE_KEYS (comma separated)
    const pks = process.env.PRIVATE_KEYS.split(",");
    const wallet = new ethers.Wallet(pks[2], provider);
    console.log("Wallet:", wallet.address);

    const WFLR = "0x1D80c49BbBCd1C0911346656B529DF9E5c2F783d";
    // Usually USDTC or USDT0, I will use Enosys V2 Router: 0xc2a013dDFaC222384a362aFd34D7a02dbE99B4c0
    
    // 1. Wrap 390 FLR
    const wflrAbi = ["function deposit() payable"];
    const wflrContract = new ethers.Contract(WFLR, wflrAbi, wallet);
    console.log("Wrapping 390 FLR...");
    const tx1 = await wflrContract.deposit({ value: ethers.utils.parseEther("390.0") });
    await tx1.wait();
    console.log("Wrapped!");

    // 2. We need the USDT0 address. Let's read it from the bot's source code!
    const fs = require('fs');
    const source = fs.readFileSync("/root/bot_enhanced/src/bin/bot3_momentum_v2.rs", "utf8");
    const usdtMatch = source.match(/const USDT0:\s*&str\s*=\s*"([^"]+)"/);
    const routerMatch = source.match(/const ENOSYS_V2_ROUTER:\s*&str\s*=\s*"([^"]+)"/);
    
    const USDT0 = usdtMatch ? usdtMatch[1] : "0x0B38e83B86d491735fEaa0a791b65c2B60c1148e"; // default bridging usdt
    const ROUTER = routerMatch ? routerMatch[1] : "0xc2a013dDFaC222384a362aFd34D7a02dbE99B4c0";

    console.log("USDT0:", USDT0);
    console.log("Router:", ROUTER);

    // 3. Approve WFLR
    const erc20Abi = ["function approve(address spender, uint256 amount) returns (bool)", "function balanceOf(address) view returns (uint256)"];
    const wflrErc = new ethers.Contract(WFLR, erc20Abi, wallet);
    console.log("Approving WFLR...");
    const tx2 = await wflrErc.approve(ROUTER, ethers.constants.MaxUint256);
    await tx2.wait();
    console.log("Approved!");

    // 4. Swap 200 WFLR to USDT0
    const routerAbi = ["function swapExactTokensForTokens(uint amountIn, uint amountOutMin, address[] calldata path, address to, uint deadline) external returns (uint[] memory amounts)"];
    const routerContract = new ethers.Contract(ROUTER, routerAbi, wallet);
    console.log("Swapping WFLR -> USDT0...");
    const amountIn = ethers.utils.parseEther("200.0");
    const deadline = Math.floor(Date.now() / 1000) + 600;
    const tx3 = await routerContract.swapExactTokensForTokens(
        amountIn,
        0, // accept any slippage for funding
        [WFLR, USDT0],
        wallet.address,
        deadline
    );
    await tx3.wait();
    console.log("Swap complete!");
    
    const usdtContract = new ethers.Contract(USDT0, erc20Abi, wallet);
    const bal = await usdtContract.balanceOf(wallet.address);
    // USDT0 usually has 6 decimals
    console.log("Final USDT0 Balance:", ethers.utils.formatUnits(bal, 6));
}
main().catch(console.error);
