//! Structural durable and session model-selection routes.

use super::command;

command!(model, 200, "model", ["models"], "Change the durable default model", [Model, Owner], false, optional("[model]") => |host, selector| host.model(selector));
command!(switch, 210, "switch", [], "Change this session's model", [Model, Session], false, required("<model>") => |host, selector| host.switch(selector));
