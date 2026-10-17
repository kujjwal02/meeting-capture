# Google setup

Sign-in needs one OAuth client in a personal Google Cloud project. The repo is public, so the
client stays out of it: you load it in the settings window.

## Create the client

1. Open <https://console.cloud.google.com/projectcreate> with the personal account. Name the
   project `meeting-capture`.
2. Enable the Google Calendar API: <https://console.cloud.google.com/apis/library/calendar-json.googleapis.com>.
3. Open Google Auth Platform, then Branding. Set the app name to "Meeting Capture" and add your
   email as the support and developer contact.
4. Under Audience, pick "External". Then press "Publish app" so the status reads "In production".
   A client in "Testing" expires refresh tokens after 7 days.
5. Under Data Access, add the scopes `.../auth/calendar.events.readonly`, `openid` and
   `.../auth/userinfo.email`.
6. Under Clients, create a client of type "Desktop app". Download its JSON file.

## Load it in the app

1. In the tray menu, open Settings.
2. Under "Google OAuth client", load the JSON file, then press Save. The client ID and the
   secret go in `settings.toml`.
3. Press "Add Google account". Sign in with akaiketech.com first, so it wins tied matches.
4. Google warns that the app is unverified. Press Advanced, then "Go to Meeting Capture".
   Tick the calendar box on the consent screen; without it the app refuses the sign-in.
5. Press "Add Google account" again for the personal account.

Delete the downloaded JSON file afterwards.

Saving a new client ID removes the old secret and signs every account out, since Google ties
both to the client. Enter the new secret and add the accounts again.

## Upgrading from a keychain build

Secrets live in `settings.toml`, and the app no longer reads the keychain. Enter the client
secret and the API key again, then sign in to each account. Delete the old
`com.ujjwalkumar.meetingcapture*` items in Keychain Access.

## Work account

The akaiketech.com Workspace admin may block unverified third-party apps. If sign-in fails with
"access blocked", ask the admin to trust the client ID under Security, API controls. The other
way out is a second OAuth client with an "Internal" consent screen in the akaiketech.com org.
