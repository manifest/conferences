table! {
    use diesel::sql_types::*;
    use crate::db::sql::*;

    janus_handle_shadow (handle_id, rtc_id) {
        handle_id -> Int8,
        rtc_id -> Uuid,
        reply_to -> Agent_id,
    }
}

table! {
    use diesel::sql_types::*;
    use crate::db::sql::*;

    janus_session_shadow (rtc_id) {
        rtc_id -> Uuid,
        session_id -> Int8,
        location_id -> Agent_id,
    }
}

table! {
    use diesel::sql_types::*;
    use crate::db::sql::*;

    room (id) {
        id -> Uuid,
        time -> Tstzrange,
        audience -> Text,
    }
}

table! {
    use diesel::sql_types::*;
    use crate::db::sql::*;

    rtc (id) {
        id -> Uuid,
        room_id -> Uuid,
    }
}

joinable!(janus_handle_shadow -> rtc (rtc_id));
joinable!(janus_session_shadow -> rtc (rtc_id));
joinable!(rtc -> room (room_id));

allow_tables_to_appear_in_same_query!(janus_handle_shadow, janus_session_shadow, room, rtc,);
