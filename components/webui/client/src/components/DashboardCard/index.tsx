import {
    Card,
    Typography,
} from "antd";

import styles from "./index.module.css";


const {Text} = Typography;

interface DashboardCardProps {
    title: string;
    titleColor?: string;
    backgroundColor?: string;
    children?: React.ReactNode;
    isLoading?: boolean;
}

/**
 * Renders a card for dashboard.
 *
 * @param props
 * @param props.title
 * @param props.titleColor
 * @param props.backgroundColor
 * @param props.children
 * @param props.isLoading
 * @return
 */
const DashboardCard = ({
    title,
    titleColor,
    backgroundColor,
    children,
    isLoading = false,
}: DashboardCardProps) => {
    return (
        <Card
            className={styles["card"] || ""}
            hoverable={true}
            loading={isLoading}
            style={{backgroundColor}}
        >
            <div className={styles["cardContent"]}>
                <Text
                    className={styles["title"] || ""}
                    style={{color: titleColor}}
                >
                    {title}
                </Text>
                {children}
            </div>
        </Card>
    );
};

export {DashboardCard};

export type {DashboardCardProps};
