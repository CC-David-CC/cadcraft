//! CADCraft drawing operations through the WebOS account host.

use std::{cell::RefCell, rc::Rc};

use cadcraft_engine::{Session, cmd::file::PreparedAccountSave};
use cadcraft_ui_egui::{AccountDrawingFn, AccountDrawingRequest, CadApp};
use serde_json::{Value, json};
use wasm_bindgen::JsValue;

use crate::account_port::{AccountPort, field, string};

pub(crate) struct AccountView {
    port: AccountPort,
    action: Rc<RefCell<Option<AccountDrawingRequest>>>,
    last_state: Value,
    pending: Option<Save>,
    dialog: Option<(String, Option<u64>)>,
    sequence: u64,
    lease: Option<String>,
    finalized: bool,
}

struct Save {
    ticket: String,
    document: String,
    revision: String,
    path: String,
    project: PreparedAccountSave,
}

impl AccountView {
    pub(crate) fn new(port: AccountPort) -> Self {
        Self { port, action: Rc::default(), last_state: Value::Null, pending: None, dialog: None, sequence: 0, lease: None, finalized: false }
    }

    pub(crate) fn drawing_service(&self) -> AccountDrawingFn {
        let action = self.action.clone();
        Box::new(move |request| {
            let mut next = action.try_borrow_mut().map_err(|_| "CADCraft is processing a project request.")?;
            if next.is_some() {
                return Err("CADCraft is processing a project request.".into());
            }
            *next = Some(request);
            Ok(())
        })
    }

    pub(crate) fn suspended(&self) -> bool {
        self.lease.is_some() || self.finalized
    }

    fn busy(&self) -> bool {
        self.pending.is_some() || self.dialog.is_some()
    }

    fn unfinished(app: &CadApp) -> bool {
        app.session.running.is_some()
            || app.session.editing_mtext().is_some()
            || app.session.state().is_ok_and(|drawing| drawing.block_edit.is_some() || drawing.doc.refedit.is_some())
    }

    fn state(&self, app: &CadApp) -> Value {
        let Ok(drawing) = app.session.state() else { return json!({"available":false}) };
        let unfinished = Self::unfinished(app);
        json!({"available":true,"documentId":drawing.uid.to_string(),"revision":drawing.revision.to_string(),
            "recoveryRevision":drawing.revision.to_string(),"dirty":drawing.is_dirty(),"capturable":!unfinished,
            "unfinishedEdit":unfinished,"autosaveEligible":false,"operationPending":self.busy()})
    }

    fn targets(app: &CadApp, packet: &JsValue) -> bool {
        app.session
            .state()
            .is_ok_and(|drawing| number(packet, "documentId") == Some(drawing.uid) && number(packet, "revision") == Some(drawing.revision))
    }

    pub(crate) fn poll(&mut self, app: &mut CadApp) {
        for (kind, packet) in self.port.take_messages() {
            if let Err(error) = self.receive(app, &kind, &packet) {
                app.set_status(error);
            }
        }
        let action = self.action.borrow_mut().take();
        if let Some(action) = action
            && let Err(error) = self.project_action(app, action)
        {
            app.set_status(error);
        }
    }

    pub(crate) fn publish_state(&mut self, app: &CadApp) -> Result<(), String> {
        let state = self.state(app);
        if state != self.last_state {
            self.port.publish("project-state", state.clone(), None)?;
            self.last_state = state;
        }
        Ok(())
    }

    fn receive(&mut self, app: &mut CadApp, kind: &str, packet: &JsValue) -> Result<(), String> {
        if kind == "save-result" {
            return self.settle_save(app, packet);
        }
        let Some(id) = bounded(packet, "requestId", 36) else { return Ok(()) };
        match kind {
            "recovery-capture" => {
                let mut reply = json!({"requestId":id,"documentId":bounded(packet,"documentId",20),"revision":bounded(packet,"revision",20)});
                if Self::targets(app, packet) && !Self::unfinished(app) && !self.suspended() {
                    match app.session.capture_account_draft() {
                        Ok(bytes) => return self.port.publish("recovery-captured", reply, Some(bytes)),
                        Err(error) => reply["message"] = json!(error.to_string()),
                    }
                } else {
                    // Let the host retire this draft request before its correlated reply arrives.
                    self.publish_state(app)?;
                    reply["message"] = json!("The CADCraft project changed before recovery capture.");
                }
                self.port.publish("recovery-captured", reply, None)
            }
            "session-save" => self.capture_session_save(app, packet, id),
            "session" => self.session(app, packet, id),
            "dialog-result" => self.dialog_result(app, packet, id),
            "operation" => self.operation(app, packet, id),
            _ => Ok(()),
        }
    }

    fn next_id(&mut self) -> Result<String, String> {
        self.sequence = self.sequence.checked_add(1).ok_or("CADCraft exhausted its request identifiers.")?;
        Ok(self.sequence.to_string())
    }

    fn request_dialog(&mut self, mut packet: Value, revision: Option<u64>) -> Result<(), String> {
        let id = self.next_id()?;
        packet["requestId"] = json!(id);
        self.port.publish("dialog", packet, None)?;
        self.dialog = Some((id, revision));
        Ok(())
    }

    fn project_action(&mut self, app: &mut CadApp, action: AccountDrawingRequest) -> Result<(), String> {
        if self.suspended() || self.busy() {
            return Err("Finish the pending CADCraft project operation first.".into());
        }
        let drawing = app.session.state().map_err(|error| error.to_string())?;
        match action {
            AccountDrawingRequest::Close => self.port.publish("close-intent", json!({}), None),
            AccountDrawingRequest::New => self.request_dialog(json!({"operation":"new","settings":"{}"}), None),
            AccountDrawingRequest::Open(path) => self.request_dialog(json!({"operation":"open","path":path}), None),
            AccountDrawingRequest::Save | AccountDrawingRequest::SaveAs => {
                if Self::unfinished(app) {
                    return Err("Finish the current CADCraft drawing command before saving.".into());
                }
                if matches!(action, AccountDrawingRequest::Save)
                    && let Some(path) = drawing.path.clone()
                {
                    return self.send_save(app, path, "save", json!({}));
                }
                let suggested = drawing.path.clone().unwrap_or_else(|| "Drawing.dxf".to_owned());
                self.request_dialog(json!({"operation":"save","suggested":suggested}), app.session.state().ok().map(|drawing| drawing.revision))
            }
        }
    }

    fn dialog_result(&mut self, app: &CadApp, packet: &JsValue, id: String) -> Result<(), String> {
        if !self.dialog.as_ref().is_some_and(|(expected, _)| expected == &id) {
            return Ok(());
        }
        let Some((_, revision)) = self.dialog.take() else { return Ok(()) };
        match bounded(packet, "outcome", 16).as_deref() {
            Some("failed") => Err(bounded(packet, "message", 250).unwrap_or_else(|| "The CADCraft dialog failed.".into())),
            Some("destination") if revision.is_some() => {
                if revision != app.session.state().ok().map(|drawing| drawing.revision) || Self::unfinished(app) || self.suspended() {
                    return Err("The project changed while choosing its save destination.".into());
                }
                let path = bounded(packet, "path", 2048).ok_or("Invalid CADCraft save destination.")?;
                self.send_save(app, path, "save", json!({}))
            }
            _ => Ok(()),
        }
    }

    fn capture_session_save(&mut self, app: &CadApp, packet: &JsValue, id: String) -> Result<(), String> {
        let path = bounded(packet, "path", 2048).ok_or("Invalid CADCraft save destination.")?;
        let mut reply = json!({"requestId":id,"path":path,"documentId":bounded(packet,"documentId",20),
            "reviewedRevision":bounded(packet,"revision",20)});
        if self.busy() || self.suspended() || Self::unfinished(app) || !Self::targets(app, packet) {
            reply["outcome"] = json!("busy");
            return self.port.publish("session-save-result", reply, None);
        }
        if let Err(error) = self.send_save(app, path, "session-save-result", reply.clone()) {
            reply["outcome"] = json!("failed");
            reply["message"] = json!(error);
            self.port.publish("session-save-result", reply, None)?;
        }
        Ok(())
    }

    fn send_save(&mut self, app: &CadApp, path: String, kind: &str, mut reply: Value) -> Result<(), String> {
        let (project, bytes) = app.session.prepare_account_save(&path).map_err(|error| error.to_string())?;
        let ticket = self.next_id()?;
        let drawing = app.session.state().map_err(|error| error.to_string())?;
        let document = drawing.uid.to_string();
        let revision = drawing.revision.to_string();
        reply["outcome"] = json!("captured");
        reply["ticket"] = json!(ticket);
        reply["documentId"] = json!(document);
        reply["revision"] = json!(revision);
        reply["path"] = json!(path);
        self.port.publish(kind, reply, Some(bytes))?;
        self.pending = Some(Save { ticket, document, revision, path, project });
        Ok(())
    }

    fn settle_save(&mut self, app: &mut CadApp, packet: &JsValue) -> Result<(), String> {
        let matching = self.pending.as_ref().is_some_and(|save| {
            bounded(packet, "ticket", 20).as_deref() == Some(save.ticket.as_str())
                && bounded(packet, "documentId", 20).as_deref() == Some(save.document.as_str())
                && bounded(packet, "revision", 20).as_deref() == Some(save.revision.as_str())
                && bounded(packet, "path", 2048).as_deref() == Some(save.path.as_str())
        });
        let outcome = bounded(packet, "outcome", 16);
        if !matching || !matches!(outcome.as_deref(), Some("written" | "failed" | "unknown")) {
            return Ok(());
        }
        let Some(save) = self.pending.take() else { return Ok(()) };
        let adopted = outcome.as_deref() == Some("written") && app.session.adopt_account_save(save.project).is_ok();
        self.port.publish(
            "save-settled",
            json!({"ticket":save.ticket,"documentId":save.document,"revision":save.revision,
            "path":save.path,"adopted":adopted,"projectState":self.state(app)}),
            None,
        )
    }

    fn session(&mut self, app: &mut CadApp, packet: &JsValue, id: String) -> Result<(), String> {
        let operation = bounded(packet, "operation", 16).ok_or("Invalid CADCraft session operation.")?;
        let lease = bounded(packet, "lease", 64).filter(|value| !value.is_empty()).ok_or("Invalid CADCraft session lease.")?;
        let accepted = match operation.as_str() {
            "pause"
                if !self.suspended()
                    && !self.busy()
                    && !Self::unfinished(app)
                    && Self::targets(app, packet)
                    && field(packet, "dirty").as_bool() == app.session.state().ok().map(|drawing| drawing.is_dirty())
                    && field(packet, "unfinishedEdit").as_bool() == Some(false) =>
            {
                self.lease = Some(lease.clone());
                true
            }
            "release" | "finalize" if self.lease.as_deref() == Some(lease.as_str()) => {
                self.lease = None;
                self.finalized = operation == "finalize";
                true
            }
            _ => false,
        };
        self.port.publish(
            "session-result",
            json!({"requestId":id,"operation":operation,"lease":lease,"ok":accepted,
            "message":if accepted {""} else {"The CADCraft project changed after close review."}}),
            None,
        )
    }

    fn operation(&self, app: &mut CadApp, packet: &JsValue, id: String) -> Result<(), String> {
        let document = bounded(packet, "documentId", 20).ok_or("Invalid CADCraft project target.")?;
        let mut reply = json!({"requestId":id,"documentId":document});
        let path = field(packet, "path");
        let path = if path.is_null() { None } else { Some(path.as_string().ok_or("Invalid CADCraft project binding.")?) };
        let drawing = app.session.state().map_err(|error| error.to_string())?;
        let result = if self.suspended() || document != drawing.uid.to_string() || path != drawing.path {
            Err("The CADCraft operation does not match the current drawing.".into())
        } else {
            let command = bounded(packet, "command", 64).ok_or("Invalid CADCraft operation.")?;
            let args = bounded(packet, "args", 4096).ok_or("Invalid CADCraft operation arguments.")?;
            let args: Value = serde_json::from_str(&args).map_err(|_| "Invalid CADCraft operation arguments.")?;
            let fields = args.as_object().ok_or("CADCraft operation arguments must be an object.")?;
            match command.as_str() {
                "document.inspect" if fields.is_empty() => Ok(json!({
                    "title":drawing.title,
                    "entityCount":drawing.drawing().entity_count(),
                    "dirty":drawing.is_dirty(),
                })),
                _ if self.busy() || Self::unfinished(app) => Err("Finish the pending CADCraft drawing command first.".into()),
                "line" if fields.len() == 1 && args["points"].as_array().is_some_and(|points| points.len() == 2 && points.iter().all(point)) => {
                    app.run("line", args)
                }
                "circle"
                    if fields.len() == 2
                        && point(&args["center"])
                        && args["radius"].as_f64().is_some_and(|radius| radius.is_finite() && radius > 0.0 && radius <= 1.0e9) =>
                {
                    app.run("circle", args)
                }
                "dimlinear" if fields.len() == 3 && point(&args["p1"]) && point(&args["p2"]) && point(&args["at"]) => app.run("dimlinear", args),
                "edit.undo" if fields.is_empty() => app.run("u", args),
                _ => Err("Unsupported CADCraft drawing operation.".into()),
            }
        };
        match result {
            Ok(value) => reply["result"] = json!(value.to_string()),
            Err(error) => reply["message"] = json!(error),
        }
        self.port.publish("operation-result", reply, None)
    }
}

fn bounded(packet: &JsValue, field: &str, maximum: usize) -> Option<String> {
    string(packet, field).filter(|text| text.len() <= maximum)
}

fn number(packet: &JsValue, field: &str) -> Option<u64> {
    bounded(packet, field, 20)?.parse().ok()
}

fn point(value: &Value) -> bool {
    value
        .as_array()
        .is_some_and(|point| point.len() == 2 && point.iter().all(|axis| axis.as_f64().is_some_and(|n| n.is_finite() && n.abs() <= 1.0e9)))
}

pub(crate) async fn open(port: &AccountPort) -> Result<(Session, Value), String> {
    let request = port.start().await?;
    let mut session = Session::account_drawing();
    if let Some(bytes) = request.bytes {
        if let Err(error) = session.open_account_drawing(request.path.as_deref(), &bytes, request.recover) {
            let error = error.to_string();
            port.open_failed(&request.request_id, &error);
            return Err(error);
        }
    } else {
        session.new_drawing(request.settings["metric"].as_bool().unwrap_or(session.settings.measureinit));
    }
    let drawing = session.state().map_err(|error| error.to_string())?;
    let identity = json!({
        "requestId":request.request_id, "documentId":drawing.uid.to_string(),
        "revision":drawing.revision.to_string(), "path":drawing.path, "ok":true,
    });
    Ok((session, identity))
}
