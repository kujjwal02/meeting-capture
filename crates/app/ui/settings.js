// The settings window. Each change goes through a Rust command in settings_window.rs.
const { invoke } = window.__TAURI__.core;

const field = (id) => document.getElementById(id);

function showMessage(text, isError) {
  const message = field("message");
  message.textContent = text;
  message.classList.toggle("error", isError);
}

function renderAccounts(accounts) {
  const list = field("accounts");
  list.replaceChildren();
  if (accounts.length === 0) {
    const empty = document.createElement("li");
    empty.textContent = "No account yet.";
    empty.className = "hint";
    list.append(empty);
  }
  for (const account of accounts) {
    const row = document.createElement("li");
    const email = document.createElement("span");
    email.textContent = account.needs_sign_in ? `${account.email} (sign in again)` : account.email;
    email.classList.toggle("needs-sign-in", account.needs_sign_in);
    const remove = document.createElement("button");
    remove.type = "button";
    remove.textContent = "Remove";
    remove.addEventListener("click", () => removeAccount(account.email));
    row.append(email, remove);
    list.append(row);
  }
}

let llm = null;
let savedClient = null;

// The stored secret belongs to the saved client ID; saving another drops it.
function showClientSecretState() {
  const isSavedClient = field("client-id").value.trim() === (savedClient?.client_id ?? "");
  field("client-secret").placeholder =
    savedClient?.has_client_secret && isSavedClient ? "Saved in the settings file" : "";
}

// Shows the fields the chosen provider takes, with its default model and key state.
function showProviderFields() {
  const provider = field("llm-provider").value;
  for (const element of document.querySelectorAll("[data-for]")) {
    element.hidden = !element.dataset.for.split(" ").includes(provider);
  }
  const defaultModels = {
    anthropic: llm?.anthropic_default_model,
    "claude-code": llm?.claude_code_default_model,
  };
  field("llm-model").placeholder = defaultModels[provider] ?? "Required";
  // The stored key belongs to the saved provider and base URL; saving another drops it.
  const baseUrl =
    provider === "openai-compatible" ? field("llm-base-url").value.trim() || null : null;
  const hasKey =
    llm?.has_api_key && provider === llm?.provider && baseUrl === (llm?.base_url ?? null);
  field("llm-api-key").placeholder = hasKey
    ? "Saved in the settings file"
    : provider === "openai-compatible"
      ? "Leave blank for a local server"
      : "";
}

async function checkClaudeCode() {
  const button = field("check-claude-code");
  const status = field("claude-code-status");
  button.disabled = true;
  status.textContent = "Checking…";
  status.classList.remove("error");
  try {
    status.textContent = await invoke("claude_code_check");
  } catch (error) {
    status.textContent = String(error);
    status.classList.add("error");
  } finally {
    button.disabled = false;
  }
}

async function load() {
  try {
    const settings = await invoke("settings_load");
    field("vault-dir").value = settings.vault_dir ?? "";
    field("my-name").value = settings.my_name ?? "";
    field("time-zone").value = settings.time_zone ?? "";
    field("receiver-port").value = settings.receiver_port ?? "";
    field("client-id").value = settings.client_id ?? "";
    field("client-secret").value = "";
    savedClient = settings;
    showClientSecretState();
    field("settings-file").textContent = settings.settings_file;
    llm = settings.llm;
    field("llm-provider").value = llm.provider ?? "";
    field("llm-model").value = llm.model ?? "";
    field("llm-base-url").value = llm.base_url ?? "";
    field("llm-api-key").value = "";
    showProviderFields();
    renderAccounts(settings.accounts);
  } catch (error) {
    showMessage(String(error), true);
  }
}

async function save(event) {
  event.preventDefault();
  const port = field("receiver-port").value.trim();
  const form = {
    vault_dir: field("vault-dir").value,
    my_name: field("my-name").value,
    receiver_port: port === "" ? null : Number(port),
    time_zone: field("time-zone").value,
    client_id: field("client-id").value,
    client_secret: field("client-secret").value,
    llm_provider: field("llm-provider").value || null,
    llm_model: field("llm-model").value,
    llm_base_url: field("llm-base-url").value,
    llm_api_key: field("llm-api-key").value,
  };
  try {
    showMessage(await invoke("settings_save", { form }), false);
    await load();
  } catch (error) {
    showMessage(String(error), true);
  }
}

async function readClientFile() {
  const file = field("client-file").files[0];
  if (!file) {
    return;
  }
  try {
    const client = await invoke("client_file_read", { json: await file.text() });
    field("client-id").value = client.client_id;
    field("client-secret").value = client.client_secret;
    showClientSecretState();
    showMessage("Client loaded. Save to keep it.", false);
  } catch (error) {
    showMessage(String(error), true);
  }
}

async function addAccount() {
  const button = field("add-account");
  button.disabled = true;
  field("sign-in-status").hidden = false;
  try {
    const email = await invoke("google_account_add");
    showMessage(`Signed in as ${email}.`, false);
    await load();
  } catch (error) {
    showMessage(String(error), true);
  } finally {
    button.disabled = false;
    field("sign-in-status").hidden = true;
  }
}

async function removeAccount(email) {
  try {
    await invoke("google_account_remove", { email });
    showMessage(`Removed ${email}.`, false);
    await load();
  } catch (error) {
    showMessage(String(error), true);
  }
}

field("settings").addEventListener("submit", save);
field("client-file").addEventListener("change", readClientFile);
field("add-account").addEventListener("click", addAccount);
field("llm-provider").addEventListener("change", () => {
  // A model name belongs to one provider; the field then shows the new provider's default.
  field("llm-model").value = "";
  showProviderFields();
});
field("llm-base-url").addEventListener("input", showProviderFields);
field("client-id").addEventListener("input", showClientSecretState);
field("check-claude-code").addEventListener("click", checkClaudeCode);
load();
