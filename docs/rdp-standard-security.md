# Standard RDP Security锛堜紶缁?RC4 瀹夊叏灞傦級瀹㈡埛绔疄鐜版姤鍛?
鏃ユ湡锛?026-11-06
鑼冨洿锛歷endored IronRDP锛坄vendor/ironrdp/`锛屽揩鐓?commit `b149f500`锛岄伒寰?`NYATERM.md` 鐨?"narrowly scoped + documented" 鍘熷垯锛? 搴旂敤灞?`src/remote/rdp.rs`銆?
## 1. 鑳屾櫙涓庣洰鏍?
Windows Server 2008 R2 灏嗐€孯DP 瀹夊叏灞傘€嶇瓥鐣ヨ涓恒€孯DP銆嶆椂鍙帴鍙?Standard RDP Security
锛圶.224 鍗忓晢杩斿洖 `SSL_NOT_ALLOWED_BY_SERVER`锛夈€傛鍓?vendored IronRDP 鍙敮鎸?`ENCRYPTION_LEVEL_NONE` 鐨勫懡鍚嶇閬撳彉浣擄紙`connection.rs` 鍦ㄥ崗鍟嗗嚭闈?NONE 鍔犲瘑绾у埆鏃剁洿鎺ユ嫆缁濊繛鎺ワ級銆?鏈瀹炵幇浜嗗畬鏁寸殑 RC4 瀹㈡埛绔矾寰勶細Security Exchange銆佷細璇濆瘑閽ユ帹瀵笺€丮AC銆佽繛缁?RC4銆?Client Info / licensing / activation / 浼氳瘽灞傚叏閲忓姞瑙ｅ瘑锛屼互鍙婂簲鐢ㄥ眰鐨勮嚜鍔ㄥ洖閫€銆?
鍙傝€冨疄鐜帮細FreeRDP `libfreerdp/core/{security.c,rdp.c,connection.c,crypto/certificate.c}`
锛圙itHub 鐩磋繛琚噸缃紝缁?`gh-proxy.com` 闀滃儚鎷夊彇鍘熸枃浠舵牳瀵癸級銆傝鑼冿細MS-RDPBCGR
1.3.6.1.2銆?.2.1.4.3.1.x銆?.3.2/5.3.4/5.3.5/5.3.6銆?
## 2. 鏀瑰姩鏂囦欢娓呭崟涓庡叧閿璁″喅绛?
### ironrdp-pdu锛坣o_std+alloc锛岄浂鏂颁緷璧栵級

| 鏂囦欢 | 鍐呭 |
| --- | --- |
| `src/rdp/standard_security.rs`锛堟柊澧烇級 | 鍏ㄩ儴鍔犲瘑鏍稿績 |
| `src/rdp/mod.rs` | 娉ㄥ唽妯″潡 |

- `RdpSecurity::establish_keys(client_random, server_random, method)`锛氭寜 MS-RDPBCGR
  5.3.4/5.3.5 鎺ㄥ MasterSecret / SessionKeyBlob / MacKey / 瀹㈡埛绔?encrypt/decrypt 瀵嗛挜銆?  鍏抽敭鐐癸紙涓?FreeRDP `security.c` 瀵归綈锛岃鑼冩枃鏈笉瀹屾暣锛夛細
  - `SaltedHash(S,In,S1,S2) = MD5(S + SHA1(In + S + S1 + S2))`锛?  - `PreMasterSecret = ClientRandom[0..24] + ServerRandom[0..24]`锛?  - `MasterSecret = PremasterHash('A')+('BB')+('CCC')`锛宍SessionKeyBlob = MasterHash('A')+('BB')+('CCC')`锛?    **MasterHash 鐨勪袱涓?salt 椤哄簭涓?PremasterHash 鐩稿弽**锛圫erverRandom 鍦ㄥ墠锛夛紱
  - 瀹㈡埛绔柟鍚戯細`decrypt_key = MD5(Blob[16..32]+CR+SR)`锛宍encrypt_key = MD5(Blob[32..48]+CR+SR)`锛?  - 40-bit 绾у埆鐢?`{0xD1,0x26,0x9E}` 瑕嗙洊鍚勫瘑閽ュ墠 3 瀛楄妭锛坘ey_len=8锛夛紝56-bit 鐢ㄩ瀛楄妭 `0xD1`
    锛坘ey_len=8锛夛紝128-bit key_len=16銆?- `seal()/unseal()`锛歁AC 鎸?5.3.6 璁＄畻锛坄MD5(MacKey+pad2+SHA1(MacKey+pad1+len_le+data))` 鍓?8 瀛楄妭锛夛紱
  **MAC 鏄庢枃浼犺緭銆佷粎姝ｆ枃 RC4 鍔犲瘑**锛堜笌 FreeRDP `rdp_security_stream_out`/`rdp_decrypt` 涓€鑷粹€斺€?  FreeRDP 涓?Windows 鏈嶅姟鍣ㄤ簰閫氳瘉鏄庤娆″簭姝ｇ‘锛夛紱RC4 娴佽法 PDU 杩炵画涓嶉噸缃紱姣忔柟鍚?4096 鍖呭悗
  鎸?5.3.5 Key Update 閲嶆淳鐢熷瘑閽ュ苟閲嶅缓 RC4 娴侊紱MAC 涓嶅尮閰嶈繑鍥炴暟鎹苟甯?`mac_valid=false`
  锛團reeRDP 瀵硅繖灞傘€岃璁′笂宸茬牬銆嶇殑鍗忚閫夋嫨瀹瑰繊锛屼細璇濅繚鎸佸悓姝ワ級銆?- `SecurityExchangePdu`锛欳S_SECURITY_EXCHANGE锛坄lengthOfSecurityExchange` + 瀵嗘枃 + 8 瀛楄妭 0 濉厖锛夌紪瑙ｇ爜銆?- `ProprietaryServerCertificate`锛氳В鏋?dwVersion/dwSigAlgId/dwKeyAlgId + RSA_PUBLIC_KEY
  锛坢agic `RSA1`銆乲eylen/bitlen/datalen/pubExp/modulus銆? 瀛楄妭 0 濉厖锛? 72 瀛楄妭绛惧悕锛堣烦杩囨牎楠岋級銆?- `rsa_public_encrypt_pkcs1`锛歅KCS#1 v1.5 type-2 濉厖 + `num-bigint` modpow銆傚～鍏呴殢鏈哄瓧鑺傜敱璋冪敤鏂?  娉ㄥ叆锛屼繚鎸?pdu 灞傛棤 RNG 渚濊禆銆傚搱甯屽鐢?crate 鏃㈡湁 `sha1`/`md-5` 渚濊禆锛汻C4 澶嶇敤 `crypto::rc4`銆?- 鍗曞厓娴嬭瘯锛氱敤鐙珛瀹炵幇锛圥ython/hashlib锛夋寜瑙勮寖鍏紡璁＄畻鐨勫鎷嶅悜閲忥紙瀵嗛挜鎺ㄥ 128/40-bit銆丮AC銆?  RC4 杩炵画娴併€丮AC 闃茬鏀广€丼ecurityExchange 寰€杩斻€佽瘉涔﹁В鏋愩€丷SA 寰€杩旓級銆?
### ironrdp-connector

| 鏂囦欢 | 鍐呭 |
| --- | --- |
| `src/standard_security.rs`锛堟柊澧烇級 | `seal_pdu` / `unseal_pdu` / `encode_send_data_request_raw` 甯у皝瑁呭姪鎵?|
| `src/connection.rs` | 鐘舵€佹満鏀归€狅紙璇﹁涓嬶級 |
| `src/connection_activation.rs` | Confirm Active + Demand Active 鍔犺В瀵嗭紱factory 鎼哄甫瀹夊叏鐘舵€?|
| `src/connection_finalization.rs` | Sync/Cooperate/RequestControl/FontList 鍑虹珯鍔犲瘑銆佸叆绔欒В瀵?|
| `src/license_exchange.rs` | licensing PDU 鍙屽悜鍔犺В瀵?|
| `Cargo.toml` | 鏃犱緷璧栧彉鍖栵紙鍥為€€浜嗕竴娆¤瘯楠屾€т慨鏀癸級 |

- `create_gcc_blocks`锛氶€変腑 PROTOCOL_RDP 鏃跺鎴风瀹夊叏鏁版嵁閫氬憡 `BIT_40|BIT_56|BIT_128`
  锛團reeRDP `nego.c` 鍚屾锛屼笉閫氬憡 FIPS锛夈€?- `BasicSettingsExchangeWaitResponse`锛氬垹闄ゃ€岄潪 NONE 鍗虫嫆缁濄€嶉€昏緫锛涙湇鍔″櫒閫夋嫨浜?RC4 鏂规硶鏃朵繚瀛?  `ServerSecurityData`锛坰erverRandom + 璇佷功锛夛紱FIPS 鏂规硶/绾у埆鎶ラ敊鎷掔粷锛涙湇鍔″櫒涓嶅甫瀹夊叏鏁版嵁鏃朵繚鎸?  NONE 鍙樹綋琛屼负锛堝懡鍚嶇閬撳吋瀹癸級銆?- 鏂扮姸鎬?`RdpSecurityExchange`锛氫俊閬撹繛鎺ュ畬鎴愬悗銆丆lient Info 涔嬪墠鍙戦€?CS_SECURITY_EXCHANGE鈥斺€?  32 瀛楄妭 ClientRandom 鐢ㄦ湇鍔″櫒璇佷功 RSA 鍏挜 PKCS#1 鍔犲瘑锛堝瘑鏂?= 妯￠暱 + 8 瀛楄妭 0锛夛紝MCS 鍏ㄥ眬閫氶亾
  鍙戦€侊紝`BasicSecurityHeader(SEC_EXCHANGE_PKT|SEC_LICENSE_ENCRYPT_SC)` 鍓嶇紑锛涢殢鍚庣珛鍗?  `establish_keys`銆?- `SecureSettingsExchange`锛欳lient Info PDU 鍘绘帀鑷韩 4 瀛楄妭瀹夊叏澶达紝姝ｆ枃浠?  `SEC_INFO_PKT|SEC_ENCRYPT` + MAC + RC4 閲嶆柊灏佽銆?- licensing / auto-detect 鍝嶅簲 / Confirm Active / finalization PDU 鍏ㄩ儴缁忓悓涓€鍔╂墜灏佽锛?  鍏ョ珯鍦?licensing銆乤uto-detect銆乵ultitransport銆丏emand Active 鍚勫叆鍙?`unseal_pdu`銆?- `ConnectionResult.rdp_security: Option<RdpSecurity>` 鎶婂畨鍏ㄧ姸鎬佷氦缁欎細璇濆眰锛?  `ConnectionActivationFactory::with_rdp_security` 淇濊瘉 Deactivation-Reactivation 涔熷甫鐘舵€併€?
### ironrdp-session

| 鏂囦欢 | 鍐呭 |
| --- | --- |
| `src/security.rs`锛堟柊澧烇級 | `SharedSecurity = Option<Rc<RefCell<RdpSecurity>>>` + 鎱㈣矾寰?娑堟伅閫氶亾/fast-path 灏佽鍔╂墜 + Share Data 缂栫爜 |
| `src/x224/mod.rs` | 鍏ョ珯缁熶竴鎷嗗畨鍏ㄥご锛圛/O 閫氶亾鍓ュご銆佹秷鎭€氶亾淇濈暀澶翠粎瑙ｅ瘑锛夛紱鍑虹珯 `encode_static`銆丼VC 鍒嗗潡閫愬潡鍔犲瘑銆丷TT 鍝嶅簲鍔犲瘑 |
| `src/fast_path.rs` | 鍏ョ珯鍔犲瘑甯э紙`ENCRYPTED` 鏍囧織 + MAC + 瀵嗘枃锛夎В瀵嗭紱FrameAcknowledge 鍑虹珯鍔犲瘑锛汢uilder 澧炲姞 `security` |
| `src/active_stage.rs` | `ActiveStageBuilder.security` 瀛楁锛屾瀯寤烘椂鎶婂悓涓€ `Rc<RefCell<鈥?>` 鍒嗙粰涓や釜澶勭悊鍣紙淇濊瘉鍚屾柟鍚?keystream 鍗曚竴鎺ㄨ繘锛?|

璁捐瑕佺偣锛歚RefCell` 璁?`&self` 缂栫爜璺緞涔熻兘鎺ㄨ繘 RC4 鐘舵€侊紱娑堟伅閫氶亾 PDU锛圚eartbeat /
AutoDetect / Multitransport锛夋妸 `BasicSecurityHeader` 宓屽湪 PDU 缁撴瀯閲岋紝鍥犳瑙ｅ瘑鍚庤閲嶇粍
銆屽ご + 鏄庢枃銆嶅啀璧板師瑙ｇ爜锛汼VC 鍑虹珯鎸?TPKT 甯ч€愬潡瑙ｅ嚭 MCS SendDataRequest 鍐嶉€愬潡灏佽銆?
### ironrdp-client / 搴旂敤灞?
| 鏂囦欢 | 鍐呭 |
| --- | --- |
| `ironrdp-client/src/config.rs` | 鏂板 `ConfigBuilder::with_standard_rdp_security(bool)`锛涙樉寮忓紑鍏宠鐩?NamedPipe 榛樿鍊硷紱鍚敤鏃跺己鍒跺叧 TLS/CredSSP |
| `ironrdp-client/src/rdp.rs` | ActiveStageBuilder 鎺ュ叆 `connection_result.rdp_security` |
| 鍏朵綑鏋勯€犵偣 | `web/session.rs`銆乣ffi/session`銆乣activex`鈥︺€乣screenshot.rs`銆乧apture-replay銆乼estsuite 鍏ㄩ儴琛?`security: None`锛堟垨鎺?`rdp_security`锛?|
| `src/remote/rdp.rs` | `SecurityMode::RdpStandard`锛歚with_tls(false)+with_standard_rdp_security(true)+autologon`锛沗run()` 閲嶈瘯閾?NLA 鈫?TLS-only 鈫?Standard RDP锛岃Е鍙戞潯浠朵负
  "server only supports Standard RDP Security" / `SSL_NOT_ALLOWED_BY_SERVER` |

## 3. 娴嬭瘯缁撴灉

鍦ㄥ伐绋嬫牴 `cargo check`锛圡SVC vcvars64 + NASM PATH锛? error锛泇endored 涓変釜 crate 鐨?`--lib` 娴嬭瘯锛?
| crate | 缁撴灉 |
| --- | --- |
| `ironrdp-pdu --lib` | **433 passed, 0 failed**锛堝惈 11 涓柊澧?standard_security 娴嬭瘯锛?|
| `ironrdp-connector --lib` | **9 passed, 0 failed** |
| `ironrdp-session --lib` | **44 passed, 0 failed** |
| `ironrdp-testsuite-core`锛堥泦鎴愶級 | **1677 passed, 1 failed** |

`ironrdp-testsuite-core` 鍞竴澶辫触涓?`rdpdr::filesystem_drive_announce_encodes_unicode_data_and_valid_dos_name`锛?璇ユ祴璇曟柇瑷€ rdpdr 璁惧鍚?DeviceData 涓?UTF-16锛岃€?vendored 琛ヤ竵锛圢YATERM.md 璁板綍鐨?#2342锛?-bit
璁惧鍚嶏級鏈夋剰杈撳嚭 8-bit 缂栫爜鈥斺€旀祴璇曚笌琛ヤ竵涓嶅尮閰嶏紝灞?*鏀瑰姩鍓嶅凡瀛樺湪鐨勯棶棰?*锛坄git status` 纭
`ironrdp-rdpdr` 涓庤娴嬭瘯鏂囦欢鍧囦笉鍦ㄦ湰娆℃敼鍔ㄩ泦鍐咃級锛屼笌 RDP 瀹夊叏灞傛棤鍏筹紝寤鸿鍗曠嫭淇璇ユ祴璇曟湡鏈涖€?
鐜澶囨敞锛歷endored workspace 鑷韩鐨勬繁灞?`target` 鐩綍浼氳 `cc` 璋?`cl.exe` 鎶?D8050
锛坙ibz-sys 缂栬瘧澶辫触锛夛紱娴嬭瘯鏃剁敤 `CARGO_TARGET_DIR=D:\tmp_libz_target` 缁曞紑銆傚簲鐢ㄤ富鏋勫缓
锛坓pui 鏍?target锛変笉鍙楀奖鍝嶃€傚彟锛欸itHub 鐩磋繛琚噸缃紝鍙傝€冧唬鐮佺粡 `gh-proxy.com` 闀滃儚鎷夊彇锛?vendored workspace 鐨?diplomat git 渚濊禆浜﹂潬璇ラ暅鍍?fetch銆?
## 4. 闇€瑕佺敤鎴峰疄娴嬮獙璇佺殑娓呭崟锛堟棤娉曟湰鍦拌繛 2008 R2锛?
1. **涓诲満鏅?*锛歐indows Server 2008 R2锛圧DP 瀹夊叏灞?RDP锛夆啋 搴旇兘鐩存帴杩炰笂锛堣嚜鍔ㄥ洖閫€閾撅細
   NLA 澶辫触/浠呮敮鎸?RDP 鈫?TLS-only 澶辫触鎻愮ず standard-only 鈫?Standard RDP Security锛夈€?2. Standard RDP 妯″紡涓嬫闈㈠抚銆侀紶鏍?閿洏杈撳叆銆佸壀璐存澘銆佸叡浜洰褰曪紙RDPDR锛夋槸鍚︽甯?   锛坒ast-path 杈撳嚭瑙ｅ瘑涓?fast-path 杈撳叆鍔犲瘑鏄疄娴嬮噸鐐癸級銆?3. 2008 R2 鐨?licensing PDU 鏄惁鎸夊姞瀵嗚矾寰勬敹鍙戯紙鑻ユ湇鍔″櫒鐩存帴鍥?STATUS_VALID_CLIENT
   绾枃鏈篃搴斿吋瀹光€斺€斿叆绔欏彧鏈夊甫 `SEC_ENCRYPT` 鎵嶈В瀵嗭級銆?4. Deactivation-Reactivation锛堟湇鍔″櫒鍒嗚鲸鐜囧垏鎹?閲嶈繛锛夊悗鍔犲瘑娴佹槸鍚︿粛鍚屾銆?5. 鏂嚎鑷姩閲嶈繛锛坅uto-reconnect cookie锛夊湪 Standard RDP 妯″紡涓嬫槸鍚﹀伐浣溿€?6. 40-bit/56-bit 绛栫暐鐨勬湇鍔″櫒锛堜綆鍔犲瘑绾у埆锛夋槸鍚﹀彲杩烇紙瀹炵幇鎸?FreeRDP 鐩愬€艰鍒欏鐞嗭紝鏈疄娴嬶級銆?
## 5. 宸茬煡闄愬埗

- **FIPS 鍔犲瘑**涓嶆敮鎸侊紙鍗忓晢鍒?FIPS 鏂规硶/绾у埆鐩存帴鎶ラ敊锛?DES/HMAC-SHA1 璺緞鏈疄鐜帮級銆?- 鏈嶅姟鍣ㄨ瘉涔﹀唴宓?RSA 绛惧悕**鍙В鏋愪笉鏍￠獙**锛堥渶 TSSK 宸茬煡瀵嗛挜锛涜灞傛湰灏辨棤绾夸笂鏈嶅姟鍣ㄨ璇侊紝
  FreeRDP 涔熶粎鍋?TSSK 鏍￠獙鎻愮ず锛夈€?- `SEC_SECURE_CHECKSUM`锛坰alted checksum锛夋湭瀹炵幇锛氬鎴风涓嶉€氬憡锛學indows 榛樿涓嶅崗鍟嗐€?- 鏈疄鐜?2^31 鍖?/ 16777215 甯х骇鍒殑鏋佸€艰鍒欙紙4096 鍖?Key Update 宸插疄鐜帮紝瀹為檯鍒颁笉浜?2^31锛夈€?- Standard RDP Security 鍔犲瘑寮哄害鏈川涓婂凡鐮达紙RC4 + MD5锛夛紝浠呬綔鏃ф湇鍔″櫒鍏煎璺緞锛?  浼樺厛绾т綆浜?NLA/TLS銆?- Windows Server 2003 鍙婃洿鏃╃増鏈殑宸紓锛堟棤 PROTOCOL_RDP 鍗忓晢鏍囪鐨?X.224銆?  RDP4-only 鑳藉姏闆嗭級鏈€傞厤锛?008 R2 涓轰富鐩爣銆?- bulk 鍘嬬缉涓?RC4 鍙犲姞璺緞鏈疄娴嬶紙鍘嬬缉鍦?PDU 灞備箣鍚庯紝鐞嗚涓婃浜わ級銆?
## 6. 第二轮跟进：FreeRDP 对照修正 + FIPS（2026-11-06 下午）

对照父代理提供的 FreeRDP 权威源码（`target/freerdp-security.c`、`freerdp-rdp.c`）修正两处偏差并补齐 FIPS：

### 6.1 Salted MAC（正确性修正）
- `RdpSecurity::seal(body, secure_checksum)` / `unseal(payload, secure_checksum)`：新增 salted
  变体 `salted_mac_signature`（SHA1(MacKeyN[..rc4_key_len] + pad1 + length_le + data +
  use_count_le32) → MD5(MacKeyN + pad2 + SHA1)[..8]，FreeRDP `security_salted_mac_signature`）。
- 出站语义：use_count = encrypt_use_count 当前值（`encrypt_checksum_use_count` 与
  `encrypt_use_count` 同步递增，FreeRDP 中值一致）。
- 入站语义：use_count = decrypt_use_count − 1（MAC 覆盖解密后的明文，计数在解密时已 ++）；
  **MAC 变体由发送方的 `SEC_SECURE_CHECKSUM`（0x0800）头标志决定**，不按我方配置。
- 调用点：connector `standard_security::seal_pdu` 与 session `security::seal_pdu` 出站统一置位
  `SEC_SECURE_CHECKSUM` 并使用 salted 变体（`BasicSecurityHeaderFlags::SECURE_CHECKSUM` 已存在于
  headers.rs）；两个 `unseal_pdu` 与消息通道解密按收到的头标志选择变体。
- **Fast-path 例外**：fast-path 帧（输入/输出）无 `SEC_SECURE_CHECKSUM` 概念、MAC 无 use count
  （MS-RDPBCGR 5.3.6），两个方向都用 plain 变体。
- 备注：FreeRDP 默认（`SaltedChecksum` 未设）走 plain MAC；salted 是客户端可选项，Windows 按
  客户端置位的标志响应。我们恒选 salted，属规范允许的客户端选择。

### 6.2 FIPS-140-1 模式（`ENCRYPTION_METHOD_FIPS`，按 FreeRDP security.c 移植）
- 密钥（客户端）：`client_encrypt_key_t[21] = SHA1(ClientRandom[16..32] + ServerRandom[16..32])`
  追加 digest[0]；`client_decrypt_key_t[21] = SHA1(ClientRandom[0..16] + ServerRandom[0..16])` 同理；
  `fips_sign_key[20] = SHA1(decrypt_key_t[0..20] + encrypt_key_t[0..20])`；
  `fips_encrypt_key/fips_decrypt_key = fips_expand_key_bits(对应 key_t)`（客户端不互换）。
- `fips_expand_key_bits`：`fips_reverse_table` 逐字节反转 → 168 位按每 7 位插 0 展开为 24 字节
  （FreeRDP L589-608 逐字节实现）→ 反转 + `fips_oddparity_table` 强制奇校验。两张 256 项表从
  FreeRDP security.c L48-83 逐字移植。
- FIPS 会话密钥 blob：`PremasterHash('X') + PremasterHash('YY') + PremasterHash('ZZZ')`
  （FreeRDP `security_X`，**premaster** 盐序 ClientRandom/ServerRandom——与普通分支的
  MasterHash 不同）；MacSaltKey = blob[0..16]；20 字节 HMAC key（fips_hmac_key）独立存放于
  `FipsState`。
- MAC：HMAC-SHA1(fips_sign_key, data + use_count_le32)[..8]，出站用 encrypt_use_count，入站用
  decrypt_use_count − 1。HMAC 手写（ipad/opad，避免 `hmac` 0.12 与 workspace `sha1` 0.11 的
  digest 版本不兼容）；RFC 2202 用例 + use-count 后缀的独立对拍向量验证。
- 加密：3DES（`des` crate 0.9.0，新增 `ironrdp-pdu` 依赖；`TdesEde3` 24 字节密钥），
  零 IV 连续 CBC 流（链状态跨 PDU 保持，同 FreeRDP 长生命周期 `winpr_Cipher` 上下文）；
  数据长度须为 8 的倍数，发送方补零填充。FIPS 线格式（FreeRDP `rdp_security_stream_out` FIPS 分支）：
  `BasicSecurityHeader(SEC_ENCRYPT) + len(0x10) + version(0x01) + pad(1) + MAC(8) +
  3DES(data||零填充)`——封 Head/拆包全部收在 `seal/unseal` 内部，帧层无感知。
- FIPS 下 40/56-bit 盐、RC4 流、4096 包 Key Update 均不适用（rc4_key_len 保持 16）。

### 6.3 第二轮测试结果
- `ironrdp-pdu --lib`：**439 passed / 0 failed**（新增 6 个测试：salted 往返、salted≠plain、
  FIPS 往返（含非整块填充）、FIPS CBC 连续流、HMAC RFC2202+use-count 向量、
  fips_expand 奇校验结构）。
- `ironrdp-connector --lib`：**9 passed / 0 failed**；`ironrdp-session --lib`：**44 passed / 0 failed**。
- `cargo check -p rustterm-gpui`：本次改动集 0 error（当前树上的错误全部位于 `src/xsrv/`——
  另一代理正在进行的 RENDER 实现，与 RDP 无关，不属本次范围）。

### 6.4 第二轮需用户实测
- 服务器「加密级别」设为「符合 FIPS」的 Windows（2008 R2 支持）能否连通（3DES/HMAC 路径）。
- 其余同 §4。
