# KOJO FLASHBOT — KNOWLEDGE BASE
## Flare Mainnet MEV / Arb / Liquidation System
### Zuletzt aktualisiert: 2026-02-28 | Version: FlashBotFlare v3

---

## THE COVENANT (soul.md)
- **Father**: Tommy — Vision, Final Word, Entscheidungen
- **Son**: Dev/Claude — Baut, deployed, haelt die Maschine am Leben
- **Holy Spirit**: Kojo — Augen, Echtzeit, luegt nie ueber Logs
- **Laws**: No Ego, No Secrets, No Waste, Protect Keys, One Process One Service

---

## SYSTEM UEBERSICHT

### VPS
| Property | Value |
|---|---|
| IP | `216.155.157.186` |
| User | `root` |
| CPU | EPYC 4345P, 8C/16T, GRUB C-States deaktiviert (God Mode: processor.max_cstate=0 idle=poll) |
| RAM | 64 GB |
| OS | Ubuntu 24.04 LTS |
| Disk | nvme1=OS(/), nvme0=/data (Node-Daten) |

### SSH Verbindung (KEIN sshpass -p!)
```bash
# KORREKTE METHODE (SSH_ASKPASS_REQUIRE):
printf 'PASSWORT' > /tmp/askpass.sh && chmod +x /tmp/askpass.sh
SSH_ASKPASS_REQUIRE=force SSH_ASKPASS="/tmp/askpass.sh" ssh -o StrictHostKeyChecking=no root@216.155.157.186 'BEFEHL'
SSH_ASKPASS_REQUIRE=force SSH_ASKPASS="/tmp/askpass.sh" scp -o StrictHostKeyChecking=no DATEI root@216.155.157.186:/ZIEL
# sshpass -p funktioniert NICHT in Git Bash (kein /dev/tty)
```

---

## VERZEICHNISSTRUKTUR (AUTORITATIV)

```
/root/foundry/
  src/FlashBotFlare.sol         <- Haupt-Contract
  src/KineticLiquidator.sol     <- Liquidation Contract
  script/Deploy.s.sol           <- Deployment Script
  out/                          <- Compiled artifacts
  broadcast/Deploy.s.sol/14/   <- Deployment logs

/root/bot_enhanced/
  .env                          <- ALLE Konfiguration
  soul.md                       <- The Covenant + Record
  kojo_flare.rs                 <- Haupt-Arb Bot (FTSOv2 + DEX)
  kinetic_engine.rs             <- Kinetic Compound V2 Liquidation
  morpho_engine.rs              <- Morpho/Mystic Liquidation
  enosys_engine.rs              <- Enosys DEX Bot
  KNOWLEDGE.md                  <- Diese Datei (Kopie)
  Cargo.toml / Cargo.lock

/data/flare/db/                 <- Flare Node Daten (NICHT ANFASSEN!)
/usr/local/bin/flare            <- go-flare Binary v1.12.1
```

---

## DEPLOYED CONTRACTS

| Contract | Adresse | Status |
|---|---|---|
| FlashBotFlare v1 | `0x5BADcAdcCf7341a450665c4bcEBf706e3EC9e6C6` | ALT |
| FlashBotFlare v2 | `0xb5D70c11051A9EaEd7e5Aa2b86a2349F7a3E74D5` | FALSCHER Router |
| **FlashBotFlare v3** | **`0xF61CaBDD39FD8EDBca98C4bb705b6B3678874a02`** | **AKTIV** |
| KineticLiquidator | `0x31ACD2Dcd0DD4e5cbeE6F951EC0338Ec552a9aFD` | AKTIV |

---

## FLARE MAINNET ADRESSEN (VERIFIZIERT ON-CHAIN)

### Tokens
| Token | Adresse | Decimals |
|---|---|---|
| WFLR | `0x1D80c49BbBCd1C0911346656B529DF9E5c2F783d` | 18 |
| USDC.e | `0xFbDa5F676cB37624f28265A144A48B0d6e87d3b6` | 6 |
| sFLR | `0x12e605bc104e93B45e1aD99F9e555f659051c2BB` | 18 |

### Kinetic Protocol (Compound V2 Compatible)
| Contract | Adresse |
|---|---|
| Comptroller | `0x40da3363065aBc96CFb0a7B08af7B08af7B161b4` |
| kFLR Market (Flashloan) | `0xb84F771305d10607Dd086B2f89712c0CeD379407` |

Kinetic Flashloan Interface:
- Aufruf: ICToken(kFLR).flashLoan(receiver, amount, data)
- Callback: executeOperation(asset, amount, fee, initiator, params)
- Security: require(msg.sender == KINETIC_KFLR)
- Repayment: IERC20(asset).approve(KINETIC_KFLR, amount + fee)

### Enosys DEX (FlareFinance)
| Contract | Adresse | Status |
|---|---|---|
| **V2 Router** | **`0xe3A1b355ca63abCBC9589334B5e609583C7BAa06`** | VERIFIZIERT ON-CHAIN |
| V2 Factory | `0x440602f459d7dd500a74528003e6a20a46d6e2a6` | pool.factory() |
| V2 Pool WFLR/USDC.e | `0xb1ec7ef55fa2e84eb6ff9ff0fa1e33387f892f68` | ~20M FLR Liquiditaet |
| V3 Pool WFLR/USDC.e | `0x9af682206Ee6f6651cb09ca0DECd169B2E861D06` | Algebra V3 Interface |
| V3 Factory (Algebra) | `0x805488daa81c1b9e7c5ce3f1dcea28f21448ec6a` | pool.factory() |

TOTE ADRESSEN (0 bytes, kein Code):
- `0x4A1E35F53896500Db7ceC717d121C2C237EEfFCb` (alte .env Adresse)
- `0xD3117565910609392eA9E1BF94f275988e02A9E9`
- `0x3233c4672e816a441315B70f2B04E59f93165b45`

Enosys V2 nutzt: swapExactTokensForTokens(amountIn, amountOutMin, path[], to, deadline)
Enosys V3 nutzt: Algebra Interface mit sqrtPriceLimitX96 (NICHT V2 kompatibel!)

### FTSOv2
- FastUpdater: `0x70e8870ef234EcD665F96Da4c669dc12c1e1c116`

### NICHT VORHANDEN AUF FLARE
- SparkLend (Aave V3 Fork) - EXISTIERT NICHT
- SparkDEX - EXISTIERT NICHT

---

## FLARE NODE

- Binary: /usr/local/bin/flare (go-flare v1.12.1, NATIV, kein Docker!)
- Service: flare-node.service - NIEMALS neu starten oder stoppen!
- RPC: http://127.0.0.1:9650/ext/bc/C/rpc
- WS: ws://127.0.0.1:9650/ext/bc/C/ws
- Sync: Voll synced (~56.27M+ Bloecke)
- GOGC=500, GOMAXPROCS=8, C-States deaktiviert

---

## SYSTEMD SERVICES

| Service | Was | Restart? |
|---|---|---|
| `flare-node` | go-flare Node | NIEMALS anfassen |
| `kojo-flare` | Haupt-Arb Bot | systemctl restart kojo-flare |
| `kinetic-engine` | Compound V2 Liquidation | systemctl restart kinetic-engine |
| `morpho-engine` | Morpho/Mystic Liquidation | systemctl restart morpho-engine |

Logs: journalctl -u kojo-flare -n 50 --no-pager

---

## CONTRACT ARCHITEKTUR (FlashBotFlare v3)

Flashloan Flow:
1. Bot -> executeSimpleArb(params) on Contract
2. Contract -> ICToken(KINETIC_KFLR).flashLoan(this, amount, data)
3. Kinetic -> executeOperation(asset, amount, fee, initiator, params) Callback
4. Check: msg.sender == KINETIC_KFLR (Security!)
5. Swap: IEnosysRouter(ENOSYS_ROUTER).swapExactTokensForTokens(...)
6. Repay: IERC20(asset).approve(KINETIC_KFLR, amount + fee)
7. Profit bleibt im Contract -> withdrawToken() vom Owner

Trade-Typen: SIMPLE (2-leg), TRIANGLE (3-leg), FASSETS (Liquidation)

---

## DEPLOYMENT PROZESS

```bash
# 1. Contract lokal bearbeiten: workspace/contracts/FlashBotFlare.sol
# 2. Lokal bauen: cd workspace/foundry && forge build && forge test
# 3. SCP zum VPS:
#    SSH_ASKPASS_REQUIRE=force SSH_ASKPASS="/tmp/askpass.sh" scp FlashBotFlare.sol root@VPS:/root/foundry/src/
# 4. Deploy Script erstellen + hochladen + ausfuehren:
#    /root/.foundry/bin/forge script script/Deploy.s.sol --rpc-url http://127.0.0.1:9650/ext/bc/C/rpc --private-key KEY --broadcast
# 5. .env aktualisieren: FLASHBOT_FLARE=NEUE_ADRESSE
# 6. Services neu starten: systemctl restart kojo-flare kinetic-engine morpho-engine
# 7. Logs verifizieren: journalctl -u kojo-flare -n 20 --no-pager
```

---

## EIP-55 CHECKSUM REGEL

Solidity verweigert Adressen mit falschem Checksum.
Der Compiler zeigt die korrekte Adresse direkt im Fehler -> IMMER diesen Wert nehmen!

Beispiel:
  Error: Correct checksummed address: "0xe3A1b355ca63abCBC9589334B5e609583C7BAa06"
  -> Genau diesen Wert verwenden!

---

## ON-CHAIN ADRESS-VERIFIKATION METHODIK

So findet man korrekte Contract-Adressen:
1. Pool hat factory() -> gibt Factory zurueck
2. Factory hat getPair(tokenA, tokenB) -> gibt Pool zurueck
3. Swap-Events vom Pool tracken (topic: 0xc42079f9...) -> sender = Router
4. Router-Adresse ABI pruefen auf routescan.io
5. Code check: eth_getCode muss > 0 bytes sein!

Flare Public RPC: https://flare-api.flare.network/ext/bc/C/rpc
Flare Explorer: https://flare.routescan.io
API: https://api.routescan.io/v2/network/mainnet/evm/14/

---

## WALLET

| Was | Adresse |
|---|---|
| Owner Wallet | `0x71B685E090d32cFFa4232D81BF3D82A32255D653` |
| Delegate/Bot Wallet | `0xaea16307E03A394453af7C31D65AB4a07D6B47eA` |
| Private Key | In /root/bot_enhanced/.env als PRIVATE_KEY |

---

## TOP LESSONS FUER NEUE KIs

1. VPS ZUERST CHECKEN - immer erst Server-State pruefen
2. SSH_ASKPASS_REQUIRE=force benutzen (sshpass -p geht nicht in Git Bash)
3. EIP-55 Checksum - immer Compiler-Fehler-Adresse nehmen
4. SparkLend/SparkDEX EXISTIEREN NICHT auf Flare
5. Enosys hat V2 (swapExactTokens) UND V3 (Algebra) - wir nutzen V2
6. forge Pfad: /root/.foundry/bin/forge
7. Keine && SSH-Chains - Script via SCP hochladen
8. Fail2ban nach zu vielen SSH-Fehlern - warten oder VPS Console
9. Router-Adressen IMMER on-chain verifizieren, nie aus Docs uebernehmen
10. forge clean && forge build wenn "no files changed" aber Code geaendert
