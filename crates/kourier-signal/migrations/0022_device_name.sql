-- Signal backend.db, link feedback L4 (docs/architecture.md §4.3 step 7): the name this device
-- was linked with, as the phone lists it among the linked devices. A logout the server did
-- not confirm tells the user to remove the device by this name on the phone. NULL for a
-- device linked before this column existed (the text then says "this computer").
ALTER TABLE account ADD COLUMN device_name TEXT;
