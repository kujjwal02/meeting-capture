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

async function load() {
  try {
    const settings = await invoke("settings_load");
    field("vault-dir").value = settings.vault_dir ?? "";
    field("my-name").value = settings.my_name ?? "";
    field("time-zone").value = settings.time_zone ?? "";
    field("receiver-port").value = settings.receiver_port ?? "";
    field("client-id").value = settings.client_id ?? "";
    field("client-secret").value = "";
    field("client-secret").placeholder = settings.has_client_secret ? "Saved in the keychain" : "";
    field("settings-file").textContent = settings.settings_file;
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
  };
  try {
    await invoke("settings_save", { form });
    showMessage("Saved.", false);
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
load();
