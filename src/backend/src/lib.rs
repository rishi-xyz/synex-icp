use ic_llm::{ChatMessage, Model, tool, ParameterType};
use ic_ledger_types::{AccountIdentifier, AccountBalanceArgs, MAINNET_LEDGER_CANISTER_ID, account_balance};
use ic_cdk::api::management_canister::http_request::{
    http_request, CanisterHttpRequestArgument, HttpMethod, HttpHeader, HttpResponse, TransformArgs,
};
use serde_json::Value;

// Updated system prompt with clearer instructions
const SYSTEM_PROMPT: &str = r#"
You are SyneXAI, a blockchain assistant that helps users check account balances.

When a user asks about cryptocurrency balances, you MUST use the appropriate tool:
- For ICP accounts (64-character hex strings): use lookup_icp_balance
- For Sui addresses (66-character hex strings starting with 0x): use lookup_sui_balance

IMPORTANT: Always use tools for balance queries. Never try to answer balance questions without calling the appropriate tool first.

Examples:
- "What is the balance of 0x2b4d..." → Use lookup_sui_balance tool
- "Check my Sui balance for 0x2b4d..." → Use lookup_sui_balance tool
- "Balance of account abc123..." → Use lookup_icp_balance tool

If you see a request about a balance, immediately call the appropriate tool with the provided address.
"#;

const MODEL: Model = Model::Qwen3_32B;

/// Lookup the balance of an ICP account.
async fn lookup_icp_account(account: &str) -> String {
    ic_cdk::println!("Looking up ICP account: {}", account);
    if account.len() != 64 {
        return "Account must be 64 characters long".to_string();
    }
    match AccountIdentifier::from_hex(account) {
        Ok(account) => {
            let balance = account_balance(
                MAINNET_LEDGER_CANISTER_ID,
                AccountBalanceArgs {
                    account,
                }
            ).await.expect("call to ledger failed");
            let result = format!("Balance of {} is {} ICP", account, balance);
            ic_cdk::println!("ICP lookup result: {}", result);
            result
        }
        Err(_) => "Invalid account".to_string(),
    }
}

/// Lookup the balance of a Sui account using Sui testnet RPC.
async fn lookup_sui_account(address: &str) -> String {
    ic_cdk::println!("Starting Sui balance lookup for address: {}", address);
    
    // Validate Sui address format (should be 66 characters: 0x + 64 hex chars)
    if !address.starts_with("0x") || address.len() != 66 {
        let error_msg = format!("Invalid Sui address format. Must be 66 characters starting with 0x. Received: {} (length: {})", address, address.len());
        ic_cdk::println!("Validation error: {}", error_msg);
        return error_msg;
    }
    
    // Validate hex characters
    if !address[2..].chars().all(|c| c.is_ascii_hexdigit()) {
        let error_msg = "Invalid Sui address. Contains non-hexadecimal characters".to_string();
        ic_cdk::println!("Hex validation error: {}", error_msg);
        return error_msg;
    }

    // Try mainnet first, then testnet if mainnet fails
    let endpoints = vec![
        "https://fullnode.mainnet.sui.io",
        "https://fullnode.testnet.sui.io",
    ];
    
    for (i, base_url) in endpoints.iter().enumerate() {
        let network_name = if i == 0 { "mainnet" } else { "testnet" };
        ic_cdk::println!("Trying {} endpoint: {}", network_name, base_url);
        
        // Create JSON-RPC request body for getting all balances
        let request_body = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"suix_getAllBalances","params":["{}"]}}"#,
            address
        );

        ic_cdk::println!("Request body: {}", request_body);

        let request = CanisterHttpRequestArgument {
            url: base_url.to_string(),
            method: HttpMethod::POST,
            body: Some(request_body.into_bytes()),
            max_response_bytes: Some(8192),
            transform: None, // Try without transform first
            headers: vec![
                HttpHeader {
                    name: "Content-Type".to_string(),
                    value: "application/json".to_string(),
                },
            ],
        };

        match http_request(request, 25_000_000_000).await {
            Ok((response,)) => {
                let status_code = response.status.clone();
                ic_cdk::println!("HTTP response status from {}: {}", network_name, status_code);
                
                // Fix: Compare with u32 instead of i32
                if status_code < 200u32 || status_code >= 300u32 {
                    ic_cdk::println!("Bad status code from {}, trying next endpoint", network_name);
                    continue;
                }
                
                match String::from_utf8(response.body.clone()) {
                    Ok(body) => {
                        ic_cdk::println!("Response body from {}: {}", network_name, body);
                        
                        if body.is_empty() {
                            ic_cdk::println!("Empty response from {}, trying next endpoint", network_name);
                            continue;
                        }
                        
                        match serde_json::from_str::<Value>(&body) {
                            Ok(json) => {
                                // Check for RPC errors first
                                if let Some(error) = json.get("error") {
                                    let error_msg = error.get("message")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("Unknown RPC error");
                                    let error_code = error.get("code")
                                        .and_then(|v| v.as_i64())
                                        .unwrap_or(-1);
                                    
                                    if error_code == -32602 || error_msg.contains("Invalid params") {
                                        ic_cdk::println!("Invalid params error from {}, trying next endpoint", network_name);
                                        continue;
                                    }
                                    
                                    return format!("RPC Error from {} {}: {}", network_name, error_code, error_msg);
                                }
                                
                                if let Some(result) = json.get("result") {
                                    if let Some(balances) = result.as_array() {
                                        if balances.is_empty() {
                                            let result = format!("Address {} has no balances on Sui {}", address, network_name);
                                            ic_cdk::println!("No balances found on {}: {}", network_name, result);
                                            
                                            // If no balance on mainnet, try testnet
                                            if network_name == "mainnet" {
                                                continue;
                                            }
                                            return result;
                                        }
                                        
                                        let mut balance_info = Vec::new();
                                        
                                        for balance in balances {
                                            if let (Some(coin_type), Some(total_balance)) = (
                                                balance.get("coinType").and_then(|v| v.as_str()),
                                                balance.get("totalBalance").and_then(|v| v.as_str())
                                            ) {
                                                match total_balance.parse::<u64>() {
                                                    Ok(balance_value) => {
                                                        if coin_type == "0x2::sui::SUI" {
                                                            let sui_balance = balance_value as f64 / 1_000_000_000.0;
                                                            balance_info.push(format!("{:.6} SUI", sui_balance));
                                                        } else {
                                                            let short_coin_type = if coin_type.len() > 50 {
                                                                format!("{}...{}", &coin_type[..20], &coin_type[coin_type.len()-10..])
                                                            } else {
                                                                coin_type.to_string()
                                                            };
                                                            balance_info.push(format!("{} {}", balance_value, short_coin_type));
                                                        }
                                                    }
                                                    Err(e) => {
                                                        return format!("Error parsing balance value '{}': {}", total_balance, e);
                                                    }
                                                }
                                            }
                                        }
                                        
                                        let result = if balance_info.is_empty() {
                                            format!("Address {} has balances but couldn't parse them on {}", address, network_name)
                                        } else {
                                            format!("Balances for {} on Sui {}: {}", address, network_name, balance_info.join(", "))
                                        };
                                        
                                        ic_cdk::println!("Final balance result from {}: {}", network_name, result);
                                        return result;
                                    } else {
                                        ic_cdk::println!("Unexpected result format from {}", network_name);
                                        continue;
                                    }
                                } else {
                                    ic_cdk::println!("No result field in response from {}", network_name);
                                    continue;
                                }
                            }
                            Err(e) => {
                                ic_cdk::println!("Error parsing JSON response from {}: {}", network_name, e);
                                continue;
                            }
                        }
                    }
                    Err(e) => {
                        ic_cdk::println!("Error decoding response body from {}: {:?}", network_name, e);
                        continue;
                    }
                }
            }
            Err(e) => {
                ic_cdk::println!("HTTP request failed for {}: {:?}", network_name, e);
                continue;
            }
        }
    }
    
    format!("Failed to get balance for {} from all Sui endpoints", address)
}

/// Transform function for HTTP outcalls (required for consensus)
#[ic_cdk::query]
fn transform_response(raw: TransformArgs) -> HttpResponse {
    let mut res = HttpResponse {
        status: raw.response.status.clone(),
        body: raw.response.body.clone(),
        headers: vec![],
    };
    
    // Only return the body, strip headers for consensus
    res.headers = vec![];
    res
}

// Helper function to extract address from user message
fn extract_sui_address(content: &str) -> Option<String> {
    // Look for 0x followed by 64 hex characters
    let words: Vec<&str> = content.split_whitespace().collect();
    for word in words {
        if word.starts_with("0x") && word.len() == 66 {
            // Validate it's all hex
            if word[2..].chars().all(|c| c.is_ascii_hexdigit()) {
                return Some(word.to_string());
            }
        }
    }
    None
}

// Helper function to extract ICP account from user message
fn extract_icp_account(content: &str) -> Option<String> {
    let words: Vec<&str> = content.split_whitespace().collect();
    for word in words {
        if word.len() == 64 && word.chars().all(|c| c.is_ascii_hexdigit()) {
            return Some(word.to_string());
        }
    }
    None
}

#[ic_cdk::update]
async fn chat(messages: Vec<ChatMessage>) -> String {
    ic_cdk::println!("Chat function called with {} messages", messages.len());
    
    // Get the last user message to analyze
    let last_user_message = messages.iter().rev().find_map(|msg| {
        match msg {
            ChatMessage::User { content } => Some(content.clone()),
            _ => None,
        }
    });
    
    // Debug: Print all user messages
    for (i, msg) in messages.iter().enumerate() {
        match msg {
            ChatMessage::User { content } => {
                ic_cdk::println!("User message {}: {}", i, content);
            }
            _ => {}
        }
    }
    
    // Pre-check: If we can identify a clear balance request, force tool usage
    if let Some(user_content) = &last_user_message {
        let content_lower = user_content.to_lowercase();
        let is_balance_request = content_lower.contains("balance") || 
                               content_lower.contains("amount") ||
                               content_lower.contains("sui") ||
                               content_lower.contains("icp");
        
        if is_balance_request {
            ic_cdk::println!("Detected balance request, attempting direct tool invocation");
            
            // Try to extract Sui address
            if let Some(sui_address) = extract_sui_address(user_content) {
                ic_cdk::println!("Found Sui address: {}, calling tool directly", sui_address);
                let balance_result = lookup_sui_account(&sui_address).await;
                return format!("I found the Sui address {} in your message. Here's the balance information:\n\n{}", sui_address, balance_result);
            }
            
            // Try to extract ICP account
            if let Some(icp_account) = extract_icp_account(user_content) {
                ic_cdk::println!("Found ICP account: {}, calling tool directly", icp_account);
                let balance_result = lookup_icp_account(&icp_account).await;
                return format!("I found the ICP account {} in your message. Here's the balance information:\n\n{}", icp_account, balance_result);
            }
        }
    }
    
    // Prepend the system prompt to the messages.
    let mut all_messages = vec![ChatMessage::System {
        content: SYSTEM_PROMPT.to_string(),
    }];
    all_messages.extend(messages);

    // Create tools with very explicit descriptions and examples
    let tools = vec![
        tool("lookup_icp_balance")
            .with_description("Use this tool to lookup the balance of an ICP account. Call this tool whenever a user asks about ICP balances or provides a 64-character hex string account.")
            .with_parameter(
                ic_llm::parameter("account", ParameterType::String)
                    .with_description("The ICP account identifier - exactly 64 hexadecimal characters (no 0x prefix)")
                    .is_required()
            )
            .build(),
        tool("lookup_sui_balance")
            .with_description("Use this tool to lookup the balance of a Sui address. Call this tool whenever a user asks about Sui balances or provides a 66-character hex string starting with 0x.")
            .with_parameter(
                ic_llm::parameter("address", ParameterType::String)
                    .with_description("The Sui address - exactly 66 characters starting with 0x followed by 64 hex characters")
                    .is_required()
            )
            .build()
    ];

    ic_cdk::println!("Tools configured: {} tools", tools.len());

    // Add an additional message to force tool usage if we detect a balance request
    if let Some(user_content) = &last_user_message {
        let content_lower = user_content.to_lowercase();
        if content_lower.contains("balance") || content_lower.contains("sui") {
            all_messages.push(ChatMessage::User {
                content: "Please use the appropriate lookup tool to check the balance for the address I provided.".to_string(),
            });
        }
    }

    ic_cdk::println!("Making initial LLM request with {} messages and {} tools", all_messages.len(), tools.len());
    
    let chat = ic_llm::chat(MODEL)
        .with_messages(all_messages.clone())
        .with_tools(tools);

    let response = chat.send().await;
    ic_cdk::println!("Initial LLM response received with {} tool calls", response.message.tool_calls.len());
    
    // Debug: Print the response content even if empty
    if let Some(content) = &response.message.content {
        ic_cdk::println!("LLM response content: '{}'", content);
    } else {
        ic_cdk::println!("LLM response content is None");
    }

    // Check if LLM wants to use tools
    if !response.message.tool_calls.is_empty() {
        ic_cdk::println!("Processing {} tool calls", response.message.tool_calls.len());
        
        // Add assistant message with tool calls
        all_messages.push(ChatMessage::Assistant(response.message.clone()));

        // Process each tool call
        for (i, tool_call) in response.message.tool_calls.iter().enumerate() {
            ic_cdk::println!("Processing tool call {}: {} with ID {}", i, tool_call.function.name, tool_call.id);
            
            let tool_result = match tool_call.function.name.as_str() {
                "lookup_icp_balance" => {
                    let account = tool_call.function.get("account").expect("account is required");
                    ic_cdk::println!("Calling lookup_icp_account with: {}", account);
                    lookup_icp_account(&account).await
                }
                "lookup_sui_balance" => {
                    let address = tool_call.function.get("address").expect("address is required");
                    ic_cdk::println!("Calling lookup_sui_account with: {}", address);
                    lookup_sui_account(&address).await
                }
                _ => {
                    let error_msg = format!("Unknown tool: {}", tool_call.function.name);
                    ic_cdk::println!("Unknown tool error: {}", error_msg);
                    error_msg
                }
            };

            ic_cdk::println!("Tool call {} result: {}", i, tool_result);

            // Add tool result to conversation
            all_messages.push(ChatMessage::Tool {
                content: tool_result,
                tool_call_id: tool_call.id.clone(),
            });
        }

        ic_cdk::println!("Making final LLM request with {} messages (including tool results)", all_messages.len());
        
        // Get final response from LLM with tool results
        let final_response = ic_llm::chat(MODEL)
            .with_messages(all_messages)
            .send()
            .await;

        let final_content = final_response.message.content.unwrap_or_default();
        ic_cdk::println!("Final response content: '{}'", final_content);
        final_content
    } else {
        ic_cdk::println!("No tool calls made by LLM - this suggests the model didn't recognize it should use tools");
        
        // Enhanced fallback logic
        if let Some(user_content) = &last_user_message {
            let content_lower = user_content.to_lowercase();
            
            if content_lower.contains("balance") || content_lower.contains("sui") || content_lower.contains("icp") {
                ic_cdk::println!("Detected balance request but LLM didn't use tools. Providing guidance.");
                
                let guidance = if user_content.contains("0x") {
                    "I can see you're asking about a Sui address balance. Let me help you with that. Please provide the complete Sui address (66 characters starting with 0x) and I'll look up the balance for you."
                } else if user_content.len() > 60 && user_content.chars().any(|c| c.is_ascii_hexdigit()) {
                    "I can see you're asking about a crypto balance. Please clarify if this is a Sui address (starts with 0x, 66 chars) or an ICP account (64 hex chars), and I'll look it up for you."
                } else {
                    "I can help you check cryptocurrency balances! Please provide:\n- For Sui: a 66-character address starting with 0x\n- For ICP: a 64-character hexadecimal account identifier"
                };
                
                return guidance.to_string();
            }
        }
        
        // Return the direct response, but if it's empty, provide a helpful message
        let direct_content = response.message.content.unwrap_or_default();
        if direct_content.is_empty() {
            let fallback = "I'm here to help you check blockchain balances! Please provide a Sui address (66 chars starting with 0x) or ICP account (64 hex chars) and I'll look up the balance.".to_string();
            ic_cdk::println!("Returning fallback message due to empty response");
            fallback
        } else {
            ic_cdk::println!("Direct response content: '{}'", direct_content);
            direct_content
        }
    }
}