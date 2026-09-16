ALTER TABLE `sessions` ADD `group_id` text;--> statement-breakpoint
UPDATE `sessions` SET `group_id` = (SELECT CASE WHEN json_valid(`data`) THEN json_extract(`data`, '$.group_id') ELSE NULL END FROM `session_details` WHERE `session_details`.`session_id` = `sessions`.`id`);
